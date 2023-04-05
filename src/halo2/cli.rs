use crate::halo2::synth::{keygen, make_constant, prover, verifier, Halo2Module, PrimeFieldOps};
use crate::{compile, prompt_inputs, read_inputs_from_file, Module};

use halo2_proofs::pasta::{EqAffine, Fp};
use halo2_proofs::plonk::keygen_vk;
use halo2_proofs::poly::commitment::Params;

use ark_serialize::{CanonicalDeserialize, CanonicalSerialize};
use ark_serialize::{Read, SerializationError};
use std::io::Write;

use clap::{Args, Subcommand};

use bincode::error::{DecodeError, EncodeError};
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::path::PathBuf;

#[derive(Subcommand)]
pub enum Halo2Commands {
    /// Compiles a given source file to a circuit
    Compile(Halo2Compile),
    /// Proves knowledge of witnesses satisfying a circuit
    Prove(Halo2Prove),
    /// Verifies that a proof is a correct one
    Verify(Halo2Verify),
}

#[derive(Args)]
pub struct Halo2Compile {
    /// Path to source file to be compiled
    #[arg(short, long)]
    source: PathBuf,
    /// Path to which circuit is written
    #[arg(short, long)]
    output: PathBuf,
}

#[derive(Args)]
pub struct Halo2Prove {
    /// Path to circuit on which to construct proof
    #[arg(short, long)]
    circuit: PathBuf,
    /// Path to which the proof is written
    #[arg(short, long)]
    output: PathBuf,
    /// Path to prover's input file
    #[arg(short, long)]
    inputs: Option<PathBuf>,
}

#[derive(Args)]
pub struct Halo2Verify {
    /// Path to circuit on which to construct proof
    #[arg(short, long)]
    circuit: PathBuf,
    /// Path to the proof that is being verified
    #[arg(short, long)]
    proof: PathBuf,
}

/* Implements the subcommand that compiles a vamp-ir file into a Halo2 circuit.
 */
fn compile_halo2_cmd(Halo2Compile { source, output }: &Halo2Compile) {
    println!("* Compiling constraints...");
    let unparsed_file = fs::read_to_string(source).expect("cannot read file");
    let module = Module::parse(&unparsed_file).unwrap();
    let module_3ac = compile(module, &PrimeFieldOps::<Fp>::default());

    println!("* Synthesizing arithmetic circuit...");
    let circuit = Halo2Module::<Fp>::new(module_3ac);
    let params: Params<EqAffine> = Params::new(circuit.k);
    let mut circuit_file = File::create(output).expect("unable to create circuit file");
    HaloCircuitData { params, circuit }
        .write(&mut circuit_file)
        .unwrap();

    println!("* Constraint compilation success!");
}

/* Implements the subcommand that creates a proof from interactively entered
 * inputs. */
fn prove_halo2_cmd(
    Halo2Prove {
        circuit,
        output,
        inputs,
    }: &Halo2Prove,
) {
    println!("* Reading arithmetic circuit...");
    let mut circuit_file = File::open(circuit).expect("unable to load circuit file");

    let mut expected_path_to_inputs = circuit.clone();
    expected_path_to_inputs.set_extension("inputs");

    let HaloCircuitData {
        params,
        mut circuit,
    } = HaloCircuitData::read(&mut circuit_file).unwrap();

    // Prompt for program inputs
    let var_assignments_ints = match inputs {
        Some(path_to_inputs) => {
            println!(
                "* Reading inputs from file {}...",
                path_to_inputs.to_string_lossy()
            );
            read_inputs_from_file(&circuit.module, path_to_inputs)
        }
        None => {
            if expected_path_to_inputs.exists() {
                println!(
                    "* Reading inputs from file {}...",
                    expected_path_to_inputs.to_string_lossy()
                );
                read_inputs_from_file(&circuit.module, &expected_path_to_inputs)
            } else {
                println!("* Soliciting circuit witnesses...");
                prompt_inputs(&circuit.module)
            }
        }
    };

    let mut var_assignments = HashMap::new();
    for (k, v) in var_assignments_ints {
        var_assignments.insert(k, make_constant(v));
    }

    // Populate variable definitions
    circuit.populate_variables(var_assignments);

    // Generating proving key
    println!("* Generating proving key...");
    let (pk, _vk) = keygen(&circuit, &params);

    // Start proving witnesses
    println!("* Proving knowledge of witnesses...");
    let proof = prover(circuit, &params, &pk);

    // verifier(&params, &vk, &proof);

    println!("* Serializing proof to storage...");
    let mut proof_file = File::create(output).expect("unable to create proof file");
    ProofDataHalo2 { proof }
        .serialize(&mut proof_file)
        .expect("Proof serialization failed");

    println!("* Proof generation success!");
}

/* Implements the subcommand that verifies that a proof is correct. */
fn verify_halo2_cmd(Halo2Verify { circuit, proof }: &Halo2Verify) {
    println!("* Reading arithmetic circuit...");
    let circuit_file = File::open(circuit).expect("unable to load circuit file");
    let HaloCircuitData { params, circuit } = HaloCircuitData::read(&circuit_file).unwrap();

    println!("* Generating verifying key...");
    let vk = keygen_vk(&params, &circuit).expect("keygen_vk should not fail");

    println!("* Reading zero-knowledge proof...");
    let mut proof_file = File::open(proof).expect("unable to load proof file");
    let ProofDataHalo2 { proof } = ProofDataHalo2::deserialize(&mut proof_file).unwrap();

    // Veryfing proof
    println!("* Verifying proof validity...");
    let verifier_result = verifier(&params, &vk, &proof);

    if let Ok(()) = verifier_result {
        println!("* Zero-knowledge proof is valid");
    } else {
        println!("* Result from verifier: {:?}", verifier_result);
    }
}

#[derive(CanonicalSerialize, CanonicalDeserialize)]
struct ProofDataHalo2 {
    proof: Vec<u8>,
}

/* Captures all the data required to use a Halo2 circuit. */
struct HaloCircuitData {
    params: Params<EqAffine>,
    circuit: Halo2Module<Fp>,
}

impl HaloCircuitData {
    fn read<R>(mut reader: R) -> Result<Self, DecodeError>
    where
        R: std::io::Read,
    {
        let params = Params::<EqAffine>::read(&mut reader)
            .map_err(|x| DecodeError::OtherString(x.to_string()))?;
        let circuit: Halo2Module<Fp> =
            bincode::decode_from_std_read(&mut reader, bincode::config::standard())?;
        Ok(Self { params, circuit })
    }

    fn write<W>(&self, mut writer: W) -> Result<(), EncodeError>
    where
        W: std::io::Write,
    {
        self.params
            .write(&mut writer)
            .expect("unable to create circuit file");
        bincode::encode_into_std_write(&self.circuit, &mut writer, bincode::config::standard())
            .expect("unable to create circuit file");
        Ok(())
    }
}

pub fn halo2(halo2_commands: &Halo2Commands) {
    match halo2_commands {
        Halo2Commands::Compile(args) => compile_halo2_cmd(args),
        Halo2Commands::Prove(args) => prove_halo2_cmd(args),
        Halo2Commands::Verify(args) => verify_halo2_cmd(args),
    }
}
