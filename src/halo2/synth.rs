use ff::PrimeField;
use group::ff::Field;
use halo2_proofs::arithmetic::FieldExt;
use halo2_proofs::circuit::{Cell, Layouter, SimpleFloorPlanner, Value};
use halo2_proofs::pasta::{EqAffine, Fp};
use halo2_proofs::plonk::*;
use halo2_proofs::poly::{commitment::Params, Rotation};
use halo2_proofs::transcript::{Blake2bRead, Blake2bWrite, Challenge255};
use rand_core::OsRng;

use num_bigint::{BigInt, BigUint, Sign, ToBigInt};
use num_traits::Signed;

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap};
use std::marker::PhantomData;

use crate::ast::{Expr, InfixOp, Module, Pat, TExpr, VariableId};
use crate::transform::{collect_module_variables, FieldOps};

struct PrimeFieldBincode<T>(Value<T>)
where
    T: PrimeField;

impl<T> bincode::Encode for PrimeFieldBincode<T>
where
    T: PrimeField,
    T::Repr: bincode::Encode,
{
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> core::result::Result<(), bincode::error::EncodeError> {
        let mut opt = None;
        self.0.map(|w| opt = Some(w.to_repr()));
        opt.encode(encoder)
    }
}

impl<T> bincode::Decode for PrimeFieldBincode<T>
where
    T: PrimeField,
    T::Repr: bincode::Decode,
{
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> core::result::Result<Self, bincode::error::DecodeError> {
        let opt = Option::<T::Repr>::decode(decoder)?;
        let val = if let Some(t) = opt {
            Value::known(T::from_repr(t).unwrap())
        } else {
            Value::unknown()
        };
        Ok(Self(val))
    }
}

// Make field elements from signed values
pub fn make_constant<F: FieldExt>(c: BigInt) -> F {
    let mut bytes = c.magnitude().to_bytes_le();
    bytes.resize(64, 0);
    let magnitude = F::from_bytes_wide(&bytes.try_into().unwrap());
    if c.is_positive() {
        magnitude
    } else {
        -magnitude
    }
}

/* Evaluate the given expression sourcing any variables from the given maps. */
fn evaluate_expr<F>(
    expr: &TExpr,
    defs: &mut HashMap<VariableId, TExpr>,
    assigns: &mut HashMap<VariableId, F>,
) -> F
where
    F: FieldExt + PrimeField,
{
    match &expr.v {
        Expr::Constant(c) => make_constant(c.clone()),
        Expr::Variable(v) => {
            if let Some(val) = assigns.get(&v.id) {
                // First look for existing variable assignment
                *val
            } else {
                // Otherwise compute variable from first principles
                let val = evaluate_expr(&defs[&v.id].clone(), defs, assigns);
                assigns.insert(v.id, val);
                val
            }
        }
        Expr::Negate(e) => -evaluate_expr(e, defs, assigns),
        Expr::Infix(InfixOp::Add, a, b) => {
            evaluate_expr(a, defs, assigns) + evaluate_expr(b, defs, assigns)
        }
        Expr::Infix(InfixOp::Subtract, a, b) => {
            evaluate_expr(a, defs, assigns) - evaluate_expr(b, defs, assigns)
        }
        Expr::Infix(InfixOp::Multiply, a, b) => {
            evaluate_expr(a, defs, assigns) * evaluate_expr(b, defs, assigns)
        }
        Expr::Infix(InfixOp::Divide, a, b) => {
            evaluate_expr(a, defs, assigns) * evaluate_expr(b, defs, assigns).invert().unwrap()
        }
        Expr::Infix(InfixOp::IntDivide, a, b) => {
            let op1 = BigUint::from_bytes_le(evaluate_expr(a, defs, assigns).to_repr().as_ref());
            let op2 = BigUint::from_bytes_le(evaluate_expr(b, defs, assigns).to_repr().as_ref());
            let bytes: Vec<u8> = (op1 / op2).to_bytes_le();
            let mut byte_array = [0u8; 64];
            let length = bytes.len();
            let padding = 64 - bytes.len();
            byte_array[..length].copy_from_slice(&bytes);
            byte_array[length..length + padding]
                .iter_mut()
                .for_each(|x| *x = 0);
            F::from_bytes_wide(&byte_array)
        }
        Expr::Infix(InfixOp::Modulo, a, b) => {
            let op1 = BigUint::from_bytes_le(evaluate_expr(a, defs, assigns).to_repr().as_ref());
            let op2 = BigUint::from_bytes_le(evaluate_expr(b, defs, assigns).to_repr().as_ref());
            let bytes: Vec<u8> = (op1 % op2).to_bytes_le();
            let mut byte_array = [0u8; 64];
            let length = bytes.len();
            let padding = 64 - bytes.len();
            byte_array[..length].copy_from_slice(&bytes);
            byte_array[length..length + padding]
                .iter_mut()
                .for_each(|x| *x = 0);
            F::from_bytes_wide(&byte_array)
        }
        _ => unreachable!("encountered unexpected expression: {}", expr),
    }
}

#[derive(Default)]
pub struct PrimeFieldOps<F>
where
    F: PrimeField,
{
    phantom: PhantomData<F>,
}

impl<F> FieldOps for PrimeFieldOps<F>
where
    F: PrimeField + FieldExt,
{
    /* Evaluate the given negation expression in the given prime field. */
    fn canonical(&self, a: BigInt) -> BigInt {
        let b = make_constant::<F>(a);
        BigUint::from_bytes_le(b.to_repr().as_ref())
            .to_bigint()
            .unwrap()
    }
    /* Evaluate the given negation expression in the given prime field. */
    fn negate(&self, a: BigInt) -> BigInt {
        let b = make_constant::<F>(a);
        BigUint::from_bytes_le((-b).to_repr().as_ref())
            .to_bigint()
            .unwrap()
    }
    /* Evaluate the given infix expression in the given prime field. */
    fn infix(&self, op: InfixOp, a: BigInt, b: BigInt) -> BigInt {
        let c = make_constant::<F>(a.clone());
        let d = make_constant::<F>(b.clone());
        match op {
            InfixOp::Add => BigUint::from_bytes_le((c + d).to_repr().as_ref())
                .to_bigint()
                .unwrap(),
            InfixOp::Subtract => BigUint::from_bytes_le((c - d).to_repr().as_ref())
                .to_bigint()
                .unwrap(),
            InfixOp::Multiply => BigUint::from_bytes_le((c * d).to_repr().as_ref())
                .to_bigint()
                .unwrap(),
            InfixOp::Divide => BigUint::from_bytes_le((c * d.invert().unwrap()).to_repr().as_ref())
                .to_bigint()
                .unwrap(),
            InfixOp::DivideZ => {
                if d == F::zero() {
                    BigInt::from(0)
                } else {
                    BigUint::from_bytes_le((c * d.invert().unwrap()).to_repr().as_ref())
                        .to_bigint()
                        .unwrap()
                }
            }
            InfixOp::IntDivide => a / b,
            InfixOp::Modulo => a % b,
            InfixOp::Exponentiate => {
                let (sign, limbs) = b.to_u64_digits();
                BigUint::from_bytes_le(
                    if sign == Sign::Minus {
                        c.pow(&limbs.try_into().unwrap()).invert().unwrap()
                    } else {
                        c.pow(&limbs.try_into().unwrap())
                    }
                    .to_repr()
                    .as_ref(),
                )
                .to_bigint()
                .unwrap()
            }
            InfixOp::Equal => panic!("cannot evaluate equals expression"),
        }
    }
}

/// This represents an advice column at a certain row in the ConstraintSystem
#[derive(Copy, Clone, Debug)]
pub struct Variable(Column<Advice>, usize);

#[derive(Clone)]
pub struct PlonkConfig {
    a: Column<Advice>,
    b: Column<Advice>,
    c: Column<Advice>,

    sl: Column<Fixed>,
    sr: Column<Fixed>,
    so: Column<Fixed>,
    sm: Column<Fixed>,
    sc: Column<Fixed>,
}

trait StandardCs<FF: FieldExt> {
    fn raw_multiply<F>(
        &self,
        layouter: &mut impl Layouter<FF>,
        f: F,
    ) -> Result<(Cell, Cell, Cell), Error>
    where
        F: FnMut() -> Value<(Assigned<FF>, Assigned<FF>, Assigned<FF>)>;
    fn raw_add<F>(
        &self,
        layouter: &mut impl Layouter<FF>,
        f: F,
    ) -> Result<(Cell, Cell, Cell), Error>
    where
        F: FnMut() -> Value<(Assigned<FF>, Assigned<FF>, Assigned<FF>)>;
    fn raw_poly<F>(
        &self,
        layouter: &mut impl Layouter<FF>,
        f: F,
    ) -> Result<(Cell, Cell, Cell), Error>
    where
        F: FnMut() -> PolyGate<Assigned<FF>>;
    fn copy(&self, layouter: &mut impl Layouter<FF>, a: Cell, b: Cell) -> Result<(), Error>;
}

#[derive(Clone)]
pub struct Halo2Module<F: PrimeField> {
    pub module: Module,
    pub variable_map: HashMap<VariableId, Value<F>>,
    pub k: u32,
}

impl<F> bincode::Encode for Halo2Module<F>
where
    F: PrimeField,
    F::Repr: bincode::Encode,
{
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> core::result::Result<(), bincode::error::EncodeError> {
        let mut encoded_variable_map = HashMap::new();
        for (k, v) in self.variable_map.clone() {
            encoded_variable_map.insert(k, PrimeFieldBincode(v));
        }
        encoded_variable_map.encode(encoder)?;
        self.module.encode(encoder)?;
        self.k.encode(encoder)?;
        Ok(())
    }
}

impl<F> bincode::Decode for Halo2Module<F>
where
    F: PrimeField,
    F::Repr: bincode::Decode,
{
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> core::result::Result<Self, bincode::error::DecodeError> {
        let encoded_variable_map = HashMap::<VariableId, PrimeFieldBincode<F>>::decode(decoder)?;
        let mut variable_map = HashMap::new();
        for (k, v) in encoded_variable_map {
            variable_map.insert(k, v.0);
        }
        let module = Module::decode(decoder)?;
        let k = u32::decode(decoder)?;
        Ok(Halo2Module {
            module,
            variable_map,
            k,
        })
    }
}

struct StandardPlonk<F: FieldExt> {
    config: PlonkConfig,
    _marker: PhantomData<F>,
}

impl<FF: FieldExt> StandardPlonk<FF> {
    fn new(config: PlonkConfig) -> Self {
        StandardPlonk {
            config,
            _marker: PhantomData,
        }
    }
}

#[derive(Copy, Clone, Debug)]
struct PolyGate<F> {
    a: Value<F>,
    b: Value<F>,
    c: Value<F>,
    q_m: F,
    q_l: F,
    q_r: F,
    q_o: F,
    q_c: F,
}

impl<FF: FieldExt> StandardCs<FF> for StandardPlonk<FF> {
    fn raw_multiply<F>(
        &self,
        layouter: &mut impl Layouter<FF>,
        mut f: F,
    ) -> Result<(Cell, Cell, Cell), Error>
    where
        F: FnMut() -> Value<(Assigned<FF>, Assigned<FF>, Assigned<FF>)>,
    {
        layouter.assign_region(
            || "raw_multiply",
            |mut region| {
                let mut value = None;
                let lhs = region.assign_advice(
                    || "lhs",
                    self.config.a,
                    0,
                    || {
                        value = Some(f());
                        value.unwrap().map(|v| v.0)
                    },
                )?;
                let rhs = region.assign_advice(
                    || "rhs",
                    self.config.b,
                    0,
                    || value.unwrap().map(|v| v.1),
                )?;
                let out = region.assign_advice(
                    || "out",
                    self.config.c,
                    0,
                    || value.unwrap().map(|v| v.2),
                )?;

                region.assign_fixed(|| "a", self.config.sl, 0, || Value::known(FF::zero()))?;
                region.assign_fixed(|| "b", self.config.sr, 0, || Value::known(FF::zero()))?;
                region.assign_fixed(|| "c", self.config.so, 0, || Value::known(FF::one()))?;
                region.assign_fixed(|| "a * b", self.config.sm, 0, || Value::known(FF::one()))?;
                Ok((lhs.cell(), rhs.cell(), out.cell()))
            },
        )
    }
    fn raw_add<F>(
        &self,
        layouter: &mut impl Layouter<FF>,
        mut f: F,
    ) -> Result<(Cell, Cell, Cell), Error>
    where
        F: FnMut() -> Value<(Assigned<FF>, Assigned<FF>, Assigned<FF>)>,
    {
        layouter.assign_region(
            || "raw_add",
            |mut region| {
                let mut value = None;
                let lhs = region.assign_advice(
                    || "lhs",
                    self.config.a,
                    0,
                    || {
                        value = Some(f());
                        value.unwrap().map(|v| v.0)
                    },
                )?;
                let rhs = region.assign_advice(
                    || "rhs",
                    self.config.b,
                    0,
                    || value.unwrap().map(|v| v.1),
                )?;
                let out = region.assign_advice(
                    || "out",
                    self.config.c,
                    0,
                    || value.unwrap().map(|v| v.2),
                )?;

                region.assign_fixed(|| "a", self.config.sl, 0, || Value::known(FF::one()))?;
                region.assign_fixed(|| "b", self.config.sr, 0, || Value::known(FF::one()))?;
                region.assign_fixed(|| "c", self.config.so, 0, || Value::known(FF::one()))?;
                region.assign_fixed(|| "a + b", self.config.sm, 0, || Value::known(FF::zero()))?;
                Ok((lhs.cell(), rhs.cell(), out.cell()))
            },
        )
    }
    fn raw_poly<F>(
        &self,
        layouter: &mut impl Layouter<FF>,
        mut f: F,
    ) -> Result<(Cell, Cell, Cell), Error>
    where
        F: FnMut() -> PolyGate<Assigned<FF>>,
    {
        layouter.assign_region(
            || "raw_poly",
            |mut region| {
                let value = f();
                let lhs = region.assign_advice(|| "lhs", self.config.a, 0, || value.a)?;
                let rhs = region.assign_advice(|| "rhs", self.config.b, 0, || value.b)?;
                let out = region.assign_advice(|| "out", self.config.c, 0, || value.c)?;

                region.assign_fixed(|| "a", self.config.sl, 0, || Value::known(value.q_l))?;
                region.assign_fixed(|| "b", self.config.sr, 0, || Value::known(value.q_r))?;
                region.assign_fixed(|| "c", self.config.so, 0, || Value::known(value.q_o))?;
                region.assign_fixed(|| "a * b", self.config.sm, 0, || Value::known(value.q_m))?;
                region.assign_fixed(|| "q_c", self.config.sc, 0, || Value::known(value.q_c))?;
                Ok((lhs.cell(), rhs.cell(), out.cell()))
            },
        )
    }
    fn copy(&self, layouter: &mut impl Layouter<FF>, left: Cell, right: Cell) -> Result<(), Error> {
        layouter.assign_region(|| "copy", |mut region| region.constrain_equal(left, right))
    }
}

impl<F: FieldExt + PrimeField> Halo2Module<F> {
    /* Make new circuit with default assignments to all variables in module. */
    pub fn new(module: Module) -> Self {
        let mut variables = HashMap::new();
        collect_module_variables(&module, &mut variables);
        let mut variable_map = HashMap::new();
        for variable in variables.keys() {
            variable_map.insert(*variable, Value::unknown());
        }
        // Computed by getting size of empty circuit
        const ROW_PADDING: usize = 8;
        let mut circuit_size = module.exprs.len() + ROW_PADDING;
        let mut k = 0;
        while circuit_size > 0 {
            circuit_size >>= 1;
            k += 1;
        }
        Self {
            module,
            variable_map,
            k,
        }
    }

    /* Populate input and auxilliary variables from the given program inputs. */
    pub fn populate_variables(&mut self, mut field_assigns: HashMap<VariableId, F>) {
        // Get the definitions necessary to populate auxiliary variables
        let mut definitions = HashMap::new();
        for def in &self.module.defs {
            if let Pat::Variable(var) = &def.0 .0.v {
                definitions.insert(var.id, *def.0 .1.clone());
            }
        }
        // Start deriving witnesses
        for (var, value) in &mut self.variable_map {
            let var_expr = Expr::Variable(crate::ast::Variable::new(*var)).type_expr(None);
            *value = Value::known(evaluate_expr(
                &var_expr,
                &mut definitions,
                &mut field_assigns,
            ));
        }
    }

    fn make_gate(
        &self,
        a: Option<VariableId>,
        b: Option<VariableId>,
        c: Option<VariableId>,
        sl: F,
        sr: F,
        so: F,
        sm: F,
        sc: F,
        cell0: Cell,
        inputs: &mut BTreeMap<VariableId, Cell>,
        cs: &impl StandardCs<F>,
        layouter: &mut impl Layouter<F>,
    ) -> Result<(), Error> {
        let (c1, c2, c3) = cs.raw_poly(layouter, || {
            let a: Value<Assigned<_>> = a
                .map(|v1| self.variable_map[&v1])
                .unwrap_or(Value::known(F::zero()))
                .into();
            let b: Value<Assigned<_>> = b
                .map(|v2| self.variable_map[&v2])
                .unwrap_or(Value::known(F::zero()))
                .into();
            let c: Value<Assigned<_>> = c
                .map(|v3| self.variable_map[&v3])
                .unwrap_or(Value::known(F::zero()))
                .into();
            PolyGate {
                a,
                b,
                c,
                q_l: sl.into(),
                q_r: sr.into(),
                q_o: so.into(),
                q_m: sm.into(),
                q_c: sc.into(),
            }
        })?;
        if let Some(v1) = a {
            copy_variable(v1, c1, inputs, cs, layouter)?;
        } else {
            cs.copy(layouter, c1, cell0)?;
        }
        if let Some(v2) = b {
            copy_variable(v2, c2, inputs, cs, layouter)?;
        } else {
            cs.copy(layouter, c2, cell0)?;
        }
        if let Some(v3) = c {
            copy_variable(v3, c3, inputs, cs, layouter)?;
        } else {
            cs.copy(layouter, c3, cell0)?;
        }
        Ok(())
    }
}

fn copy_variable<F: FieldExt>(
    var: VariableId,
    cell: Cell,
    map: &mut BTreeMap<VariableId, Cell>,
    cs: &impl StandardCs<F>,
    layouter: &mut impl Layouter<F>,
) -> Result<(), Error> {
    match map.entry(var) {
        Entry::Vacant(vac) => {
            vac.insert(cell);
        }
        Entry::Occupied(occ) => cs.copy(layouter, cell, *occ.get())?,
    }
    Ok(())
}

impl<F: FieldExt + Field> Circuit<F> for Halo2Module<F> {
    type Config = PlonkConfig;
    type FloorPlanner = SimpleFloorPlanner;

    fn without_witnesses(&self) -> Self {
        let mut variable_map = self.variable_map.clone();
        for val in variable_map.values_mut() {
            *val = Value::unknown();
        }
        Self {
            variable_map,
            module: self.module.clone(),
            k: self.k,
        }
    }

    fn configure(meta: &mut ConstraintSystem<F>) -> PlonkConfig {
        meta.set_minimum_degree(5);

        let a = meta.advice_column();
        let b = meta.advice_column();
        let c = meta.advice_column();

        meta.enable_equality(a);
        meta.enable_equality(b);
        meta.enable_equality(c);

        let sm = meta.fixed_column();
        let sl = meta.fixed_column();
        let sr = meta.fixed_column();
        let so = meta.fixed_column();
        let sc = meta.fixed_column();

        meta.create_gate("Combined add-mult", |meta| {
            let a = meta.query_advice(a, Rotation::cur());
            let b = meta.query_advice(b, Rotation::cur());
            let c = meta.query_advice(c, Rotation::cur());

            let sl = meta.query_fixed(sl, Rotation::cur());
            let sr = meta.query_fixed(sr, Rotation::cur());
            let so = meta.query_fixed(so, Rotation::cur());
            let sm = meta.query_fixed(sm, Rotation::cur());
            let sc = meta.query_fixed(sc, Rotation::cur());

            vec![a.clone() * sl + b.clone() * sr + a * b * sm + (c * so) + sc]
        });

        PlonkConfig {
            a,
            b,
            c,
            sl,
            sr,
            so,
            sm,
            sc,
        }
    }

    fn synthesize(&self, config: PlonkConfig, mut layouter: impl Layouter<F>) -> Result<(), Error> {
        let cs = StandardPlonk::new(config);

        let mut inputs = BTreeMap::new();

        let val1: Assigned<_> = Assigned::from(F::one());
        let val0: Assigned<_> = Assigned::from(F::zero());
        let (_, cell0, _) = cs.raw_poly(&mut layouter, || PolyGate {
            a: Value::known(val0),
            b: Value::known(val0),
            c: Value::known(val0),
            q_l: val0,
            q_r: val1,
            q_o: val0,
            q_m: val0,
            q_c: val0,
        })?;

        for expr in &self.module.exprs {
            if let Expr::Infix(InfixOp::Equal, lhs, rhs) = &expr.v {
                match (&lhs.v, &rhs.v) {
                    // Variables on the LHS
                    // v1 = v2
                    (Expr::Variable(v1), Expr::Variable(v2)) => {
                        self.make_gate(
                            Some(v1.id),
                            Some(v2.id),
                            None,
                            F::one(),
                            -F::one(),
                            F::zero(),
                            F::zero(),
                            F::zero(),
                            cell0,
                            &mut inputs,
                            &cs,
                            &mut layouter,
                        )?;
                    }
                    // v1 = c2
                    (Expr::Variable(v1), Expr::Constant(c2)) => {
                        let op2: F = make_constant::<F>(c2.clone());
                        self.make_gate(
                            Some(v1.id),
                            None,
                            None,
                            F::one(),
                            F::zero(),
                            F::zero(),
                            F::zero(),
                            -op2,
                            cell0,
                            &mut inputs,
                            &cs,
                            &mut layouter,
                        )?;
                    }
                    // v1 = -c2
                    (Expr::Variable(v1), Expr::Negate(e2))
                        if matches!(&e2.v, Expr::Constant(c2) if {
                            let op2: F = make_constant::<F>(c2.clone());
                            self.make_gate(Some(v1.id), None, None, F::one(), F::zero(), F::zero(), F::zero(), op2, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = -v2
                    (Expr::Variable(v1), Expr::Negate(e2))
                        if matches!(&e2.v, Expr::Variable(v2) if {
                            self.make_gate(Some(v1.id), Some(v2.id), None, F::one(), F::one(), F::zero(), F::zero(), F::zero(), cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = c2 + c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op2: F = make_constant::<F>(c2.clone());
                            let op3: F = make_constant::<F>(c3.clone());
                            self.make_gate(Some(v1.id), None, None, F::one(), F::one(), F::zero(), F::zero(), -op2-op3, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = v2 + c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op3: F = make_constant::<F>(c3.clone());
                            self.make_gate(Some(v1.id), Some(v2.id), None, F::one(), -F::one(), F::zero(), F::zero(), -op3, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = c2 + v3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op2: F = make_constant::<F>(c2.clone());
                            self.make_gate(Some(v1.id), Some(v3.id), None, F::one(), -F::one(), F::zero(), F::zero(), -op2, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = v2 + v3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            self.make_gate(Some(v1.id), Some(v2.id), Some(v3.id), F::one(), -F::one(), -F::one(), F::zero(), F::zero(), cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = c2 - c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op2: F = make_constant::<F>(c2.clone());
                            let op3: F = make_constant::<F>(c3.clone());
                            self.make_gate(Some(v1.id), None, None, F::one(), F::zero(), F::zero(), F::zero(), op3-op2, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = v2 - c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op3: F = make_constant::<F>(c3.clone());
                            self.make_gate(Some(v1.id), Some(v2.id), None, F::one(), -F::one(), F::zero(), F::zero(), op3, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = c2 - v3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op2: F = make_constant::<F>(c2.clone());
                            self.make_gate(Some(v1.id), Some(v3.id), None, F::one(), F::one(), F::zero(), F::zero(), -op2, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = v2 - v3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            self.make_gate(Some(v1.id), Some(v2.id), Some(v3.id), F::one(), -F::one(), F::one(), F::zero(), F::zero(), cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = c2 / c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c2.clone());
                            let op2: F = make_constant(c3.clone());
                            self.make_gate(Some(v1.id), None, None, F::one(), F::zero(), F::zero(), F::zero(), -(op1*op2.invert().unwrap()), cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = v2 / c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op2: F = make_constant(c3.clone());
                            self.make_gate(Some(v1.id), Some(v2.id), None, F::one(), -op2.invert().unwrap(), F::zero(), F::zero(), F::zero(), cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = c2 / v3 ***
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op2: F = make_constant::<F>(c2.clone());
                            self.make_gate(Some(v1.id), Some(v3.id), None, F::zero(), F::zero(), F::zero(), F::one(), -op2, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = v2 / v3 ***
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            self.make_gate(Some(v1.id), Some(v3.id), Some(v2.id), F::zero(), F::zero(), -F::one(), F::one(), F::zero(), cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = c2 * c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c2.clone());
                            let op2: F = make_constant(c3.clone());
                            self.make_gate(Some(v1.id), None, None, F::one(), F::zero(), F::zero(), F::zero(), -(op1*op2), cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = v2 * c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op2: F = make_constant(c3.clone());
                            self.make_gate(Some(v1.id), Some(v2.id), None, F::one(), -op2, F::zero(), F::zero(), F::zero(), cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = c2 * v3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op2: F = make_constant(c2.clone());
                            self.make_gate(Some(v1.id), Some(v3.id), None, F::one(), -op2, F::zero(), F::zero(), F::zero(), cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // v1 = v2 * v3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            self.make_gate(Some(v2.id), Some(v3.id), Some(v1.id), F::zero(), F::zero(), F::one(), -F::one(), F::zero(), cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // Now for constants on the LHS
                    // c1 = v2
                    (Expr::Constant(c1), Expr::Variable(v2)) => {
                        let op1: F = make_constant::<F>(c1.clone());
                        self.make_gate(
                            Some(v2.id),
                            None,
                            None,
                            F::one(),
                            F::zero(),
                            F::zero(),
                            F::zero(),
                            -op1,
                            cell0,
                            &mut inputs,
                            &cs,
                            &mut layouter,
                        )?;
                    }
                    // c1 = c2
                    (Expr::Constant(c1), Expr::Constant(c2)) => {
                        let op1: F = make_constant::<F>(c1.clone());
                        let op2: F = make_constant::<F>(c2.clone());
                        self.make_gate(
                            None,
                            None,
                            None,
                            F::zero(),
                            F::zero(),
                            F::zero(),
                            F::zero(),
                            op1 - op2,
                            cell0,
                            &mut inputs,
                            &cs,
                            &mut layouter,
                        )?;
                    }
                    // c1 = -c2
                    (Expr::Constant(c1), Expr::Negate(e2))
                        if matches!(&e2.v, Expr::Constant(c2) if {
                            let op1: F = make_constant::<F>(c1.clone());
                            let op2: F = make_constant::<F>(c2.clone());
                            self.make_gate(None, None, None, F::zero(), F::zero(), F::zero(), F::zero(), op1+op2, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = -v2
                    (Expr::Constant(c1), Expr::Negate(e2))
                        if matches!(&e2.v, Expr::Variable(v2) if {
                            let op1: F = make_constant::<F>(c1.clone());
                            self.make_gate(Some(v2.id), None, None, F::one(), F::zero(), F::zero(), F::zero(), op1, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = c2 + c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant::<F>(c1.clone());
                            let op2: F = make_constant::<F>(c2.clone());
                            let op3: F = make_constant::<F>(c3.clone());
                            self.make_gate(None, None, None, F::zero(), F::zero(), F::zero(), F::zero(), op1-op2-op3, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = v2 + c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant::<F>(c1.clone());
                            let op3: F = make_constant::<F>(c3.clone());
                            self.make_gate(Some(v2.id), None, None, F::one(), F::zero(), F::zero(), F::zero(), op3-op1, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = c2 + v3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant::<F>(c1.clone());
                            let op2: F = make_constant::<F>(c2.clone());
                            self.make_gate(Some(v3.id), None, None, F::one(), F::zero(), F::zero(), F::zero(), op2-op1, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = v2 + v3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant::<F>(c1.clone());
                            self.make_gate(Some(v2.id), Some(v3.id), None, F::one(), F::one(), F::zero(), F::zero(), -op1, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = c2 - c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant::<F>(c1.clone());
                            let op2: F = make_constant::<F>(c2.clone());
                            let op3: F = make_constant::<F>(c3.clone());
                            self.make_gate(None, None, None, F::zero(), F::zero(), F::zero(), F::zero(), op1-op2+op3, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = v2 - c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant::<F>(c1.clone());
                            let op3: F = make_constant::<F>(c3.clone());
                            self.make_gate(Some(v2.id), None, None, F::one(), F::zero(), F::zero(), F::zero(), -op1-op3, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = c2 - v3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant::<F>(c1.clone());
                            let op2: F = make_constant::<F>(c2.clone());
                            self.make_gate(Some(v3.id), None, None, F::one(), F::zero(), F::zero(), F::zero(), op1-op2, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = v2 - v3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant::<F>(c1.clone());
                            self.make_gate(Some(v2.id), Some(v3.id), None, F::one(), -F::one(), F::zero(), F::zero(), -op1, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = c2 / c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c1.clone());
                            let op2: F = make_constant(c2.clone());
                            let op3: F = make_constant(c3.clone());
                            self.make_gate(None, None, None, F::zero(), F::zero(), F::zero(), F::zero(), op1*op3-op2, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = v2 / c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c1.clone());
                            let op3: F = make_constant(c3.clone());
                            self.make_gate(Some(v2.id), None, None, F::one(), F::zero(), F::zero(), F::zero(), -op1*op3, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = c2 / v3 ***
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant(c1.clone());
                            let op2: F = make_constant(c2.clone());
                            self.make_gate(Some(v3.id), None, None, op1, F::zero(), F::zero(), F::zero(), -op2, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = v2 / v3 ***
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant(c1.clone());
                            self.make_gate(Some(v2.id), Some(v3.id), None, F::one(), -op1, F::zero(), F::zero(), F::zero(), cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = c2 * c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c1.clone());
                            let op2: F = make_constant(c2.clone());
                            let op3: F = make_constant(c3.clone());
                            self.make_gate(None, None, None, F::zero(), F::zero(), F::zero(), F::zero(), op1-op2*op3, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = v2 * c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c1.clone());
                            let op3: F = make_constant(c3.clone());
                            self.make_gate(Some(v2.id), None, None, op3, F::zero(), F::zero(), F::zero(), -op1, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = c2 * v3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant(c1.clone());
                            let op2: F = make_constant(c2.clone());
                            self.make_gate(Some(v3.id), None, None, op2, F::zero(), F::zero(), F::zero(), -op1, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    // c1 = v2 * v3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant(c1.clone());
                            self.make_gate(Some(v2.id), Some(v3.id), None, F::zero(), F::zero(), F::zero(), F::one(), -op1, cell0, &mut inputs, &cs, &mut layouter)?;
                            true
                        }) => {}
                    _ => panic!("unsupported constraint encountered: {}", expr),
                }
            }
        }

        Ok(())
    }
}

pub fn keygen(
    circuit: &Halo2Module<Fp>,
    params: &Params<EqAffine>,
) -> (ProvingKey<EqAffine>, VerifyingKey<EqAffine>) {
    let vk = keygen_vk(params, circuit).expect("keygen_vk should not fail");
    let vk_return = vk.clone();
    let pk = keygen_pk(params, vk, circuit).expect("keygen_pk should not fail");
    (pk, vk_return)
}

pub fn prover(
    circuit: Halo2Module<Fp>,
    params: &Params<EqAffine>,
    pk: &ProvingKey<EqAffine>,
) -> Vec<u8> {
    let rng = OsRng;
    let mut transcript = Blake2bWrite::<_, _, Challenge255<_>>::init(vec![]);
    create_proof(params, pk, &[circuit], &[&[]], rng, &mut transcript)
        .expect("proof generation should not fail");
    transcript.finalize()
}

pub fn verifier(
    params: &Params<EqAffine>,
    vk: &VerifyingKey<EqAffine>,
    proof: &[u8],
) -> Result<(), Error> {
    let strategy = SingleVerifier::new(params);
    let mut transcript = Blake2bRead::<_, _, Challenge255<_>>::init(proof);
    verify_proof(params, vk, strategy, &[&[]], &mut transcript)
}
