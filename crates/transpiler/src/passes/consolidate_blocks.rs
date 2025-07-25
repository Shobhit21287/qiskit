// This code is part of Qiskit.
//
// (C) Copyright IBM 2024
//
// This code is licensed under the Apache License, Version 2.0. You may
// obtain a copy of this license in the LICENSE.txt file in the root directory
// of this source tree or at http://www.apache.org/licenses/LICENSE-2.0.
//
// Any modifications or derivative works of this code must retain this
// copyright notice, and modified files need to carry a notice indicating
// that they have been altered from the originals.

use hashbrown::{HashMap, HashSet};
use nalgebra::Matrix2;
use ndarray::{aview2, Array2};
use num_complex::Complex64;
use numpy::PyReadonlyArray2;
use pyo3::intern;
use pyo3::prelude::*;
use qiskit_circuit::circuit_data::CircuitData;
use qiskit_circuit::dag_circuit::DAGCircuit;
use qiskit_circuit::gate_matrix::{ONE_QUBIT_IDENTITY, TWO_QUBIT_IDENTITY};
use qiskit_circuit::imports::{QI_OPERATOR, QUANTUM_CIRCUIT};
use qiskit_circuit::operations::{ArrayType, Operation, Param, UnitaryGate};
use qiskit_circuit::packed_instruction::PackedOperation;
use qiskit_circuit::Qubit;
use rustworkx_core::petgraph::stable_graph::NodeIndex;
use smallvec::smallvec;

use super::optimize_1q_gates_decomposition::matmul_1q;
use qiskit_quantum_info::convert_2q_block_matrix::{blocks_to_matrix, get_matrix_from_inst};
use qiskit_synthesis::two_qubit_decompose::{
    TwoQubitBasisDecomposer, TwoQubitControlledUDecomposer,
};

use crate::target::Qargs;
use crate::target::Target;
use qiskit_circuit::PhysicalQubit;

#[allow(clippy::large_enum_variant)]
#[derive(Clone, Debug, FromPyObject)]
pub enum DecomposerType {
    TwoQubitBasis(TwoQubitBasisDecomposer),
    TwoQubitControlledU(TwoQubitControlledUDecomposer),
}

fn is_supported(
    target: Option<&Target>,
    basis_gates: Option<&HashSet<String>>,
    name: &str,
    qargs: &[Qubit],
) -> bool {
    match target {
        Some(target) => {
            let physical_qargs: Qargs = qargs.iter().map(|bit| PhysicalQubit(bit.0)).collect();
            target.instruction_supported(name, &physical_qargs)
        }
        None => match basis_gates {
            Some(basis_gates) => basis_gates.contains(name),
            None => true,
        },
    }
}

// If depth > 20, there will be 1q gates to consolidate.
const MAX_2Q_DEPTH: usize = 20;

#[allow(clippy::too_many_arguments)]
#[pyfunction]
#[pyo3(name = "consolidate_blocks", signature = (dag, decomposer, basis_gate_name, force_consolidate, target=None, basis_gates=None, blocks=None, runs=None))]
pub fn run_consolidate_blocks(
    py: Python,
    dag: &mut DAGCircuit,
    decomposer: DecomposerType,
    basis_gate_name: &str,
    force_consolidate: bool,
    target: Option<&Target>,
    basis_gates: Option<HashSet<String>>,
    blocks: Option<Vec<Vec<usize>>>,
    runs: Option<Vec<Vec<usize>>>,
) -> PyResult<()> {
    let blocks = match blocks {
        Some(runs) => runs
            .into_iter()
            .map(|run| {
                run.into_iter()
                    .map(NodeIndex::new)
                    .collect::<Vec<NodeIndex>>()
            })
            .collect(),
        // If runs are specified but blocks are none we're in a legacy configuration where external
        // collection passes are being used. In this case don't collect blocks because it's
        // unexpected.
        None => match runs {
            Some(_) => vec![],
            None => dag.collect_2q_runs().unwrap(),
        },
    };

    let runs: Option<Vec<Vec<NodeIndex>>> = runs.map(|runs| {
        runs.into_iter()
            .map(|run| {
                run.into_iter()
                    .map(NodeIndex::new)
                    .collect::<Vec<NodeIndex>>()
            })
            .collect()
    });
    let mut all_block_gates: HashSet<NodeIndex> =
        HashSet::with_capacity(blocks.iter().map(|x| x.len()).sum());
    let mut block_qargs: HashSet<Qubit> = HashSet::with_capacity(2);
    for block in blocks {
        block_qargs.clear();
        if block.len() == 1 {
            let inst_node = block[0];
            let inst = dag[inst_node].unwrap_operation();
            if !is_supported(
                target,
                basis_gates.as_ref(),
                inst.op.name(),
                dag.get_qargs(inst.qubits),
            ) {
                all_block_gates.insert(inst_node);
                let matrix = match get_matrix_from_inst(py, inst) {
                    Ok(mat) => mat,
                    Err(_) => continue,
                };
                // TODO: Use Matrix2/ArrayType::OneQ when we're using nalgebra
                // for consolidation
                let unitary_gate = UnitaryGate {
                    array: ArrayType::NDArray(matrix),
                };
                dag.substitute_op(
                    inst_node,
                    PackedOperation::from_unitary(Box::new(unitary_gate)),
                    smallvec![],
                    None,
                )?;
                continue;
            }
        }
        let mut basis_count: usize = 0;
        let mut outside_basis = false;
        for node in &block {
            let inst = dag[*node].unwrap_operation();
            block_qargs.extend(dag.get_qargs(inst.qubits));
            all_block_gates.insert(*node);
            if inst.op.name() == basis_gate_name {
                basis_count += 1;
            }
            if !is_supported(
                target,
                basis_gates.as_ref(),
                inst.op.name(),
                dag.get_qargs(inst.qubits),
            ) {
                outside_basis = true;
            }
        }
        if block_qargs.len() > 2 {
            let mut qargs: Vec<Qubit> = block_qargs.iter().copied().collect();
            qargs.sort();
            let block_index_map: HashMap<Qubit, usize> = qargs
                .into_iter()
                .enumerate()
                .map(|(idx, qubit)| (qubit, idx))
                .collect();
            let circuit_data = CircuitData::from_packed_operations(
                block_qargs.len() as u32,
                0,
                block.iter().map(|node| {
                    let inst = dag[*node].unwrap_operation();

                    Ok((
                        inst.op.clone(),
                        inst.params_view().iter().cloned().collect(),
                        dag.get_qargs(inst.qubits)
                            .iter()
                            .map(|x| Qubit::new(block_index_map[x]))
                            .collect(),
                        vec![],
                    ))
                }),
                Param::Float(0.),
            )?;
            let circuit = QUANTUM_CIRCUIT
                .get_bound(py)
                .call_method1(intern!(py, "_from_circuit_data"), (circuit_data,))?;
            let array = QI_OPERATOR
                .get_bound(py)
                .call1((circuit,))?
                .getattr(intern!(py, "data"))?
                .extract::<PyReadonlyArray2<Complex64>>()?;
            let matrix = array.as_array();
            let identity: Array2<Complex64> = Array2::eye(2usize.pow(block_qargs.len() as u32));
            if approx::abs_diff_eq!(identity, matrix) {
                for node in block {
                    dag.remove_op_node(node);
                }
            } else {
                let matrix = array.as_array().to_owned();
                let unitary_gate = UnitaryGate {
                    array: ArrayType::NDArray(matrix),
                };
                let clbit_pos_map = HashMap::new();
                dag.replace_block(
                    &block,
                    PackedOperation::from_unitary(Box::new(unitary_gate)),
                    smallvec![],
                    None,
                    false,
                    &block_index_map,
                    &clbit_pos_map,
                )?;
            }
        } else {
            let block_index_map = [
                *block_qargs.iter().min().unwrap(),
                *block_qargs.iter().max().unwrap(),
            ];
            let matrix = blocks_to_matrix(py, dag, &block, block_index_map).ok();
            if let Some(matrix) = matrix {
                let num_basis_gates = match decomposer {
                    DecomposerType::TwoQubitBasis(ref decomp) => {
                        decomp.num_basis_gates_inner(matrix.view())
                    }
                    DecomposerType::TwoQubitControlledU(ref decomp) => {
                        decomp.num_basis_gates_inner(matrix.view())?
                    }
                };

                if force_consolidate
                    || num_basis_gates < basis_count
                    || block.len() > MAX_2Q_DEPTH
                    || (basis_gates.is_some() && outside_basis)
                    || (target.is_some() && outside_basis)
                {
                    if approx::abs_diff_eq!(aview2(&TWO_QUBIT_IDENTITY), matrix) {
                        for node in block {
                            dag.remove_op_node(node);
                        }
                    } else {
                        // TODO: Use Matrix4/ArrayType::TwoQ when we're using nalgebra
                        // for consolidation
                        let unitary_gate = UnitaryGate {
                            array: ArrayType::NDArray(matrix),
                        };
                        let qubit_pos_map = block_index_map
                            .into_iter()
                            .enumerate()
                            .map(|(idx, qubit)| (qubit, idx))
                            .collect();
                        let clbit_pos_map = HashMap::new();
                        dag.replace_block(
                            &block,
                            PackedOperation::from_unitary(Box::new(unitary_gate)),
                            smallvec![],
                            None,
                            false,
                            &qubit_pos_map,
                            &clbit_pos_map,
                        )?;
                    }
                }
            }
        }
    }
    if let Some(runs) = runs {
        for run in runs {
            if run.iter().any(|node| all_block_gates.contains(node)) {
                continue;
            }
            let first_inst_node = run[0];
            let first_inst = dag[first_inst_node].unwrap_operation();
            let first_qubits = dag.get_qargs(first_inst.qubits);

            if run.len() == 1
                && !is_supported(
                    target,
                    basis_gates.as_ref(),
                    first_inst.op.name(),
                    first_qubits,
                )
            {
                let matrix = match get_matrix_from_inst(py, first_inst) {
                    Ok(mat) => mat,
                    Err(_) => continue,
                };
                let unitary_gate = UnitaryGate {
                    array: ArrayType::NDArray(matrix),
                };
                dag.substitute_op(
                    first_inst_node,
                    PackedOperation::from_unitary(Box::new(unitary_gate)),
                    smallvec![],
                    None,
                )?;
                continue;
            }
            let qubit = first_qubits[0];
            let mut matrix = ONE_QUBIT_IDENTITY;

            let mut already_in_block = false;
            for node in &run {
                if all_block_gates.contains(node) {
                    already_in_block = true;
                }
                let gate = dag[*node].unwrap_operation();
                let operator = match get_matrix_from_inst(py, gate) {
                    Ok(mat) => mat,
                    Err(_) => {
                        // Set this to skip this run because we can't compute the matrix of the
                        // operation.
                        already_in_block = true;
                        break;
                    }
                };
                matmul_1q(&mut matrix, operator);
            }
            if already_in_block {
                continue;
            }
            if approx::abs_diff_eq!(aview2(&ONE_QUBIT_IDENTITY), aview2(&matrix)) {
                for node in run {
                    dag.remove_op_node(node);
                }
            } else {
                let array: Matrix2<Complex64> =
                    Matrix2::from_row_iterator(matrix.into_iter().flat_map(|x| x.into_iter()));
                let unitary_gate = UnitaryGate {
                    array: ArrayType::OneQ(array),
                };
                let mut block_index_map: HashMap<Qubit, usize> = HashMap::with_capacity(1);
                block_index_map.insert(qubit, 0);
                let clbit_pos_map = HashMap::new();
                dag.replace_block(
                    &run,
                    PackedOperation::from_unitary(Box::new(unitary_gate)),
                    smallvec![],
                    None,
                    false,
                    &block_index_map,
                    &clbit_pos_map,
                )?;
            }
        }
    }

    Ok(())
}

pub fn consolidate_blocks_mod(m: &Bound<PyModule>) -> PyResult<()> {
    m.add_wrapped(wrap_pyfunction!(run_consolidate_blocks))?;
    Ok(())
}
