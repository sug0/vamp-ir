use crate::ast::Variable;
use crate::ast::{Expr, InfixOp, Module, Pat, TExpr, VariableId};
use crate::transform::{collect_module_variables, FieldOps};
use ark_ec::TEModelParameters;
use ark_ff::PrimeField;
use num_bigint::{BigInt, BigUint, Sign, ToBigInt};
use num_traits::Signed;
use plonk_core::circuit::Circuit;
use plonk_core::constraint_system::StandardComposer;
use plonk_core::error::Error;
use plonk_core::proof_system::pi::PublicInputs;
use std::collections::{BTreeMap, HashMap};
use std::marker::PhantomData;

struct PrimeFieldBincode<T>(T)
where
    T: PrimeField;

impl<T> bincode::Encode for PrimeFieldBincode<T>
where
    T: PrimeField,
{
    fn encode<E: bincode::enc::Encoder>(
        &self,
        encoder: &mut E,
    ) -> core::result::Result<(), bincode::error::EncodeError> {
        let biguint: BigUint = self.0.into();
        biguint.to_u32_digits().encode(encoder)
    }
}

impl<T> bincode::Decode for PrimeFieldBincode<T>
where
    T: PrimeField,
{
    fn decode<D: bincode::de::Decoder>(
        decoder: &mut D,
    ) -> core::result::Result<Self, bincode::error::DecodeError> {
        let digits = Vec::<u32>::decode(decoder)?;
        T::try_from(BigUint::new(digits)).map(Self).map_err(|_| {
            bincode::error::DecodeError::OtherString(
                "cannot convert from BigUint to PrimeField type".to_string(),
            )
        })
    }
}

// Make field elements from signed values
pub fn make_constant<F: PrimeField>(c: &BigInt) -> F {
    let magnitude = F::from(c.magnitude().clone());
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
    F: PrimeField,
{
    match &expr.v {
        Expr::Constant(c) => make_constant(c),
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
            evaluate_expr(a, defs, assigns) / evaluate_expr(b, defs, assigns)
        }
        Expr::Infix(InfixOp::DivideZ, a, b) => {
            let denom = evaluate_expr(b, defs, assigns);
            if denom == F::zero() {
                F::zero()
            } else {
                evaluate_expr(a, defs, assigns) / denom
            }
        }
        Expr::Infix(InfixOp::IntDivide, a, b) => {
            (Into::<BigUint>::into(evaluate_expr(a, defs, assigns))
                / Into::<BigUint>::into(evaluate_expr(b, defs, assigns)))
            .into()
        }
        Expr::Infix(InfixOp::Modulo, a, b) => {
            (Into::<BigUint>::into(evaluate_expr(a, defs, assigns))
                % Into::<BigUint>::into(evaluate_expr(b, defs, assigns)))
            .into()
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
    F: PrimeField,
{
    /* Evaluate the given negation expression in the given prime field. */
    fn canonical(&self, a: BigInt) -> BigInt {
        let b = make_constant::<F>(&a);
        Into::<BigUint>::into(b).to_bigint().unwrap()
    }
    /* Evaluate the given negation expression in the given prime field. */
    fn negate(&self, a: BigInt) -> BigInt {
        let b = make_constant::<F>(&a);
        Into::<BigUint>::into(-b).to_bigint().unwrap()
    }
    /* Evaluate the given infix expression in the given prime field. */
    fn infix(&self, op: InfixOp, a: BigInt, b: BigInt) -> BigInt {
        let c = make_constant::<F>(&a);
        let d = make_constant::<F>(&b);
        match op {
            InfixOp::Add => Into::<BigUint>::into(c + d).to_bigint().unwrap(),
            InfixOp::Subtract => Into::<BigUint>::into(c - d).to_bigint().unwrap(),
            InfixOp::Multiply => Into::<BigUint>::into(c * d).to_bigint().unwrap(),
            InfixOp::Divide => Into::<BigUint>::into(c / d).to_bigint().unwrap(),
            InfixOp::DivideZ => {
                Into::<BigUint>::into(if d == F::zero() { F::zero() } else { c / d })
                    .to_bigint()
                    .unwrap()
            }
            InfixOp::IntDivide => a / b,
            InfixOp::Modulo => a % b,
            InfixOp::Exponentiate => {
                let (sign, limbs) = b.to_u64_digits();
                Into::<BigUint>::into(if sign == Sign::Minus {
                    F::one() / c.pow(limbs)
                } else {
                    c.pow(limbs)
                })
                .to_bigint()
                .unwrap()
            }
            InfixOp::Equal => panic!("cannot evaluate equals expression"),
        }
    }
}

pub struct PlonkModule<F, P>
where
    F: PrimeField,
    P: TEModelParameters<BaseField = F>,
{
    pub module: Module,
    variable_map: HashMap<VariableId, F>,
    phantom: PhantomData<P>,
}

impl<F, P> bincode::Encode for PlonkModule<F, P>
where
    F: PrimeField,
    P: TEModelParameters<BaseField = F>,
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
        Ok(())
    }
}

impl<F, P> bincode::Decode for PlonkModule<F, P>
where
    F: PrimeField,
    P: TEModelParameters<BaseField = F>,
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
        Ok(PlonkModule {
            module,
            variable_map,
            phantom: PhantomData,
        })
    }
}

impl<F, P> PlonkModule<F, P>
where
    F: PrimeField,
    P: TEModelParameters<BaseField = F>,
{
    /* Make new circuit with default assignments to all variables in module. */
    pub fn new(module: Module) -> PlonkModule<F, P> {
        let mut variables = HashMap::new();
        collect_module_variables(&module, &mut variables);
        let mut variable_map = HashMap::new();
        for variable in variables.keys() {
            variable_map.insert(*variable, F::default());
        }
        PlonkModule {
            module,
            variable_map,
            phantom: PhantomData,
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
            *value = evaluate_expr(&var_expr, &mut definitions, &mut field_assigns);
        }
    }

    /* Annotate the given public inputs with the variable names contained in
     * this module. This function assumes that the public variables in this
     * module and the public inputs in the argument occur in the same order. */
    pub fn annotate_public_inputs(
        &self,
        intended_pi_pos: &Vec<usize>,
        pi: &PublicInputs<F>,
    ) -> HashMap<VariableId, (Variable, F)> {
        // First map public input positions to values
        let mut pi_map = BTreeMap::new();
        for (pos, val) in pi.get_pos().zip(pi.get_vals()) {
            pi_map.insert(*pos, *val);
        }
        // Next, annotate the public inputs with this module's variables
        let mut annotated = HashMap::new();
        for (var, pos) in self.module.pubs.iter().zip(intended_pi_pos) {
            let val = pi_map.get(pos).copied().unwrap_or(F::zero());
            annotated.insert(var.id, (var.clone(), val));
        }
        annotated
    }
}

impl<F, P> Circuit<F, P> for PlonkModule<F, P>
where
    F: PrimeField,
    P: TEModelParameters<BaseField = F>,
{
    const CIRCUIT_ID: [u8; 32] = [0xff; 32];

    fn gadget(&mut self, composer: &mut StandardComposer<F, P>) -> Result<(), Error> {
        let mut inputs = BTreeMap::new();
        for (var, field_elt) in &self.variable_map {
            inputs.insert(var, composer.add_input(*field_elt));
        }
        let zero = composer.zero_var();
        // It is assumed that the generated PublicInputs will share the same
        // order as this module's public variables
        for var in &self.module.pubs {
            composer.arithmetic_gate(|gate| {
                gate.witness(inputs[&var.id], zero, Some(zero))
                    .add(-F::one(), F::zero())
                    .pi(self.variable_map[&var.id])
            });
        }
        for expr in &self.module.exprs {
            if let Expr::Infix(InfixOp::Equal, lhs, rhs) = &expr.v {
                match (&lhs.v, &rhs.v) {
                    // Variables on the LHS
                    // v1 = v2
                    (Expr::Variable(v1), Expr::Variable(v2)) => {
                        composer.arithmetic_gate(|gate| {
                            gate.witness(inputs[&v1.id], inputs[&v2.id], Some(zero))
                                .add(F::one(), -F::one())
                        });
                    }
                    // v1 = c2
                    (Expr::Variable(v1), Expr::Constant(c2)) => {
                        composer.arithmetic_gate(|gate| {
                            gate.witness(inputs[&v1.id], zero, Some(zero))
                                .add(F::one(), F::zero())
                                .constant(make_constant(&-c2))
                        });
                    }
                    // v1 = -c2
                    (Expr::Variable(v1), Expr::Negate(e2))
                        if matches!(&e2.v, Expr::Constant(c2) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(make_constant(c2))
                            });
                            true
                        }) => {}
                    // v1 = -v2
                    (Expr::Variable(v1), Expr::Negate(e2))
                        if matches!(&e2.v, Expr::Variable(v2) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v2.id], Some(zero))
                                    .add(F::one(), F::one())
                            });
                            true
                        }) => {}
                    // v1 = c2 + c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(make_constant(&(-c2-c3)))
                            });
                            true
                        }) => {}
                    // v1 = v2 + c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v2.id], Some(zero))
                                    .add(F::one(), -F::one())
                                    .constant(make_constant(&-c3))
                            });
                            true
                        }) => {}
                    // v1 = c2 + v3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v3.id], Some(zero))
                                    .add(F::one(), -F::one())
                                    .constant(make_constant(&-c2))
                            });
                            true
                        }) => {}
                    // v1 = v2 + v3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v2.id], Some(inputs[&v3.id]))
                                    .add(F::one(), -F::one())
                                    .out(-F::one())
                            });
                            true
                        }) => {}
                    // v1 = c2 - c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(make_constant(&(-c2+c3)))
                            });
                            true
                        }) => {}
                    // v1 = v2 - c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v2.id], Some(zero))
                                    .add(F::one(), -F::one())
                                    .constant(make_constant(c3))
                            });
                            true
                        }) => {}
                    // v1 = c2 - v3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v3.id], Some(zero))
                                    .add(F::one(), F::one())
                                    .constant(make_constant(&-c2))
                            });
                            true
                        }) => {}
                    // v1 = v2 - v3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v2.id], Some(inputs[&v3.id]))
                                    .add(F::one(), -F::one())
                                    .out(F::one())
                            });
                            true
                        }) => {}

                    // v1 = c2 / c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c2);
                            let op2: F = make_constant(c3);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(-(op1/op2))
                            });
                            true
                        }) => {}
                    // v1 = v2 / c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op2: F = make_constant(c3);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v2.id], Some(zero))
                                    .add(F::one(), -(F::one()/op2))
                            });
                            true
                        }) => {}
                    // v1 = c2 / v3 ***
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant(c2);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v3.id], Some(zero))
                                    .mul(F::one())
                                    .constant(-op1)
                            });
                            true
                        }) => {}
                    // v1 = v2 / v3 ***
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v3.id], Some(inputs[&v2.id]))
                                    .mul(F::one())
                                    .out(-F::one())
                            });
                            true
                        }) => {}

                    // v1 = c2 | c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::DivideZ, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c2);
                            let op2: F = make_constant(c3);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(-(if op2 == F::zero() { F::zero() } else { op1/op2 }))
                            });
                            true
                        }) => {}
                    // v1 = v2 | c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::DivideZ, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op2: F = make_constant(c3);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v2.id], Some(zero))
                                    .add(F::one(), -(if op2 == F::zero() { F::zero() } else { F::one()/op2 }))
                            });
                            true
                        }) => {}
                    // v1 = c2 | v3 ***
                    (Expr::Variable(v1), Expr::Infix(InfixOp::DivideZ, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant(c2);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v3.id], Some(zero))
                                    .mul(F::one())
                                    .constant(-op1)
                            });
                            true
                        }) => {}
                    // v1 = v2 | v3 ***
                    (Expr::Variable(v1), Expr::Infix(InfixOp::DivideZ, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v3.id], Some(inputs[&v2.id]))
                                    .mul(F::one())
                                    .out(-F::one())
                            });
                            true
                        }) => {}

                    // v1 = c2 * c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c2);
                            let op2: F = make_constant(c3);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(-(op1*op2))
                            });
                            true
                        }) => {}
                    // v1 = v2 * c3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op2: F = make_constant(c3);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v2.id], Some(zero))
                                    .add(F::one(), -op2)
                            });
                            true
                        }) => {}
                    // v1 = c2 * v3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op2: F = make_constant(c2);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v1.id], inputs[&v3.id], Some(zero))
                                    .add(F::one(), -op2)
                            });
                            true
                        }) => {}
                    // v1 = v2 * v3
                    (Expr::Variable(v1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v2.id], inputs[&v3.id], Some(inputs[&v1.id]))
                                    .mul(F::one())
                                    .out(-F::one())
                            });
                            true
                        }) => {}
                    // Now for constants on the LHS
                    // c1 = v2
                    (Expr::Constant(c1), Expr::Variable(v2)) => {
                        composer.arithmetic_gate(|gate| {
                            gate.witness(inputs[&v2.id], zero, Some(zero))
                                .add(F::one(), F::zero())
                                .constant(make_constant(&-c1))
                        });
                    }
                    // c1 = c2
                    (Expr::Constant(c1), Expr::Constant(c2)) => {
                        composer.arithmetic_gate(|gate| {
                            gate.witness(zero, zero, Some(zero))
                                .add(F::zero(), F::zero())
                                .constant(make_constant(&(c2 - c1)))
                        });
                    }
                    // c1 = -c2
                    (Expr::Constant(c1), Expr::Negate(e2))
                        if matches!(&e2.v, Expr::Constant(c2) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(zero, zero, Some(zero))
                                    .add(F::zero(), F::zero())
                                    .constant(make_constant(&(c1+c2)))
                            });
                            true
                        }) => {}
                    // c1 = -v2
                    (Expr::Constant(c1), Expr::Negate(e2))
                        if matches!(&e2.v, Expr::Variable(v2) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v2.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(make_constant(c1))
                            });
                            true
                        }) => {}
                    // c1 = c2 + c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(zero, zero, Some(zero))
                                    .add(F::zero(), F::zero())
                                    .constant(make_constant(&(c1-c2-c3)))
                            });
                            true
                        }) => {}
                    // c1 = v2 + c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v2.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(make_constant(&(c3-c1)))
                            });
                            true
                        }) => {}
                    // c1 = c2 + v3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v3.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(make_constant(&(c2-c1)))
                            });
                            true
                        }) => {}
                    // c1 = v2 + v3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Add, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v2.id], inputs[&v3.id], Some(zero))
                                    .add(F::one(), F::one())
                                    .constant(make_constant(&-c1))
                            });
                            true
                        }) => {}
                    // c1 = c2 - c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(zero, zero, Some(zero))
                                    .add(F::zero(), F::zero())
                                    .constant(make_constant(&(c2-c3-c1)))
                            });
                            true
                        }) => {}
                    // c1 = v2 - c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v2.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(make_constant(&(-c3-c1)))
                            });
                            true
                        }) => {}
                    // c1 = c2 - v3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v3.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(make_constant(&(c1-c2)))
                            });
                            true
                        }) => {}
                    // c1 = v2 - v3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Subtract, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v2.id], inputs[&v3.id], Some(zero))
                                    .add(F::one(), -F::one())
                                    .constant(make_constant(&-c1))
                            });
                            true
                        }) => {}

                    // c1 = c2 / c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c1);
                            let op2: F = make_constant(c2);
                            let op3: F = make_constant(c3);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(zero, zero, Some(zero))
                                    .add(F::zero(), F::zero())
                                    .constant(op1-(op2/op3))
                            });
                            true
                        }) => {}
                    // c1 = v2 / c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c1);
                            let op3: F = make_constant(c3);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v2.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(-(op1*op3))
                            });
                            true
                        }) => {}
                    // c1 = c2 / v3 ***
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant(c1);
                            let op2: F = make_constant(c2);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v3.id], zero, Some(zero))
                                    .constant(-(op2/op1))
                            });
                            true
                        }) => {}
                    // c1 = v2 / v3 ***
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Divide, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant(c1);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v2.id], inputs[&v3.id], Some(zero))
                                    .add(F::one(), -op1)
                            });
                            true
                        }) => {}

                    // c1 = c2 | c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::DivideZ, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c1);
                            let op2: F = make_constant(c2);
                            let op3: F = make_constant(c3);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(zero, zero, Some(zero))
                                    .add(F::zero(), F::zero())
                                    .constant(op1-(if op3 == F::zero() { F::zero() } else { op2/op3 }))
                            });
                            true
                        }) => {}
                    // c1 = v2 | c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::DivideZ, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c1);
                            let op3: F = make_constant(c3);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v2.id], zero, Some(zero))
                                    .add(F::one(), F::zero())
                                    .constant(-(op1*op3))
                            });
                            true
                        }) => {}
                    // c1 = c2 | v3 ***
                    (Expr::Constant(c1), Expr::Infix(InfixOp::DivideZ, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant(c1);
                            let op2: F = make_constant(c2);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v3.id], zero, Some(zero))
                                    .constant(-(if op1 == F::zero() { F::zero() } else { op2/op1 }))
                            });
                            true
                        }) => {}
                    // c1 = v2 | v3 ***
                    (Expr::Constant(c1), Expr::Infix(InfixOp::DivideZ, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant(c1);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v2.id], inputs[&v3.id], Some(zero))
                                    .add(F::one(), -op1)
                            });
                            true
                        }) => {}

                    // c1 = c2 * c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c1);
                            let op2: F = make_constant(c2);
                            let op3: F = make_constant(c3);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(zero, zero, Some(zero))
                                    .add(F::zero(), F::zero())
                                    .constant(op1-(op2*op3))
                            });
                            true
                        }) => {}
                    // c1 = v2 * c3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Constant(c3),
                        ) if {
                            let op1: F = make_constant(c1);
                            let op3: F = make_constant(c3);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v2.id], zero, Some(zero))
                                    .add(op3, F::zero())
                                    .constant(-op1)
                            });
                            true
                        }) => {}
                    // c1 = c2 * v3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Constant(c2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant(c1);
                            let op2: F = make_constant(c2);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v3.id], zero, Some(zero))
                                    .add(op2, F::zero())
                                    .constant(-op1)
                            });
                            true
                        }) => {}
                    // c1 = v2 * v3
                    (Expr::Constant(c1), Expr::Infix(InfixOp::Multiply, e2, e3))
                        if matches!((&e2.v, &e3.v), (
                            Expr::Variable(v2),
                            Expr::Variable(v3),
                        ) if {
                            let op1: F = make_constant(c1);
                            composer.arithmetic_gate(|gate| {
                                gate.witness(inputs[&v2.id], inputs[&v3.id], Some(zero))
                                    .mul(F::one())
                                    .constant(-op1)
                            });
                            true
                        }) => {}
                    _ => panic!("unsupported constraint encountered: {}", expr),
                }
            }
        }
        Ok(())
    }

    fn padded_circuit_size(&self) -> usize {
        // The with_expected_size function adds the following gates:
        // 1 gate to constrain the zero variable to equal 0
        // 3 gates to add blinging factors to the circuit polynomials
        const BUILTIN_GATE_COUNT: usize = 4;
        (self.module.exprs.len() + self.module.pubs.len() + BUILTIN_GATE_COUNT).next_power_of_two()
    }
}
