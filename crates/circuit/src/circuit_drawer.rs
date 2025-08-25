// This code is part of Qiskit.
//
// (C) Copyright IBM 2023, 2024
//
// This code is licensed under the Apache License, Version 2.0. You may
// obtain a copy of this license in the LICENSE.txt file in the root directory
// of this source tree or at http://www.apache.org/licenses/LICENSE-2.0.
//
// Any modifications or derivative works of this code must retain this
// copyright notice, and modified files need to carry a notice indicating
// that they have been altered from the originals.

use std::fmt::Debug;
use std::hash::{Hash, RandomState};
#[cfg(feature = "cache_pygates")]
use std::sync::OnceLock;
use std::thread::current;
use hashbrown::HashSet;
use crate::bit::{
    BitLocations, ClassicalRegister, PyBit, QuantumRegister, Register, ShareableClbit,
    ShareableQubit,
};
use crate::bit_locator::BitLocator;
use crate::circuit_instruction::{CircuitInstruction, OperationFromPython};
use crate::classical::expr;
use crate::dag_circuit::{self, add_global_phase, DAGCircuit, DAGStretchType, DAGVarType};
use crate::imports::{ANNOTATED_OPERATION, QUANTUM_CIRCUIT};
use rustworkx_core::petgraph::stable_graph::{EdgeReference, NodeIndex};
use crate::interner::{Interned, Interner};
use crate::object_registry::ObjectRegistry;
use crate::operations::{Operation, OperationRef, Param, PythonOperation, StandardGate};
use crate::packed_instruction::{PackedInstruction, PackedOperation};
use crate::parameter_table::{ParameterTable, ParameterTableError, ParameterUse, ParameterUuid};
use crate::register_data::RegisterData;
use crate::slice::{PySequenceIndex, SequenceIndex};
use crate::{Clbit, Qubit, Stretch, Var, VarsMode};

use numpy::PyReadonlyArray1;
use pyo3::exceptions::*;
use pyo3::prelude::*;
use pyo3::{import_exception};


use crate::converters::circuit_to_dag;
use crate::converters::QuantumCircuitData;
use crate::dag_circuit::NodeType;
use crate::circuit_data::CircuitData;

import_exception!(qiskit.circuit.exceptions, CircuitError);

#[pyclass(sequence, module = "qiskit._accelerate.circuit")]
pub struct CircuitDrawer;

#[pymethods]
impl CircuitDrawer{

    #[staticmethod]
    #[pyo3(name = "draw")]
    fn py_drawer(py: Python, quantum_circuit: &Bound<PyAny>) -> PyResult<()> {
        if !quantum_circuit.is_instance(QUANTUM_CIRCUIT.get_bound(py))? {
            return Err(PyTypeError::new_err(
                "Expected a QuantumCircuit instance"
            ));
        }
        println!("FUNCTION IS BEING CALLED FROM circuit_drawer.rs FILE");
        let circ_data: CircuitData = quantum_circuit.getattr("_data")?.extract()?;
        circuit_draw(&circ_data);
        Ok(())
    }
}

// pub struct InstructionIndices {
//     pub instruction: NodeIndex,
//     pub qubit_indices: (u32,u32),

// }

pub const q_wire: &str = "─";
pub const c_wire: char = '═';
pub const top_con: char = '┴';
pub const bot_con: char = '┬';
pub const left_con: char = '┤';
pub const right_con: char = '├';
pub const top_left_con: char = '┌';
pub const top_right_con: char = '┐';
pub const bot_left_con: char = '└';
pub const bot_right_con: char = '┘';

#[derive(Clone)]
pub struct qubit_wire {
    pub top: String,
    pub mid: String,
    pub bot: String,
    pub wire_len: u64
}

impl qubit_wire {
    pub fn new() -> Self {
        qubit_wire {
            top: String::new(),
            mid: String::new(),
            bot: String::new(),
            wire_len: 0
        }
    }

    pub fn update_wire_len(&mut self) {
        let top_len = self.top.len();
        let mid_len = self.mid.len();
        let bot_len = self.bot.len();
        if top_len == mid_len && mid_len == bot_len {
            self.wire_len = top_len as u64;
        } else {
            panic!("The lengths of the wire components are not equal");
        }
    }

    // setting qubit names
    pub fn qubit_name(&mut self, qubit_name: &str) {
        let name_len = qubit_name.len();
        self.top.push_str(" ".repeat(name_len).as_str());
        self.mid.push_str(&format!("{}", qubit_name));
        self.bot.push_str(" ".repeat(name_len).as_str());
        self.update_wire_len();
    }

    // concatenate full wire representation and send for printing
    pub fn get_wire_rep(&self) -> String {
        let mut wire_rep = String::new();
        wire_rep.push_str(&self.top);
        wire_rep.push('\n');
        wire_rep.push_str(&self.mid);
        wire_rep.push('\n');
        wire_rep.push_str(&self.bot);
        wire_rep.push('\n');
        wire_rep
    }

    pub fn fix_len(&mut self, num: u64, chr: &str) {
        self.wire_len = self.wire_len + num;
        self.top.push_str(" ".repeat(num as usize).as_str());
        self.mid.push_str(chr.repeat(num as usize).as_str());
        self.bot.push_str(" ".repeat(num as usize).as_str());
    }
}


pub struct circuit_rep {
    q_wires: Vec::<qubit_wire>,
    dag_circ: DAGCircuit
}

impl circuit_rep {
    pub fn new(dag_circ: DAGCircuit) -> Self {

        //number of qubits in dag_circuit
        let qubit = dag_circ.num_qubits();

        circuit_rep {
            q_wires: vec!(qubit_wire::new(); qubit as usize),
            dag_circ: dag_circ
        }
    }

    pub fn circuit_string(&self) -> String {
        let mut output = String::new();
        for wires in self.q_wires.iter() {
            output.push_str(&wires.get_wire_rep());
        }
        output
    }

    pub fn fix_len(&mut self, chr: &str) {
        let mut num = 0;
        for wire in self.q_wires.iter() {
            if wire.wire_len > num {
                num = wire.wire_len;
            }
        }

        for wire in self.q_wires.iter_mut() {
            wire.fix_len(num - wire.wire_len, chr);
        }
    }

    pub fn set_qubit_name(&mut self) {
        for (i, qubit) in self.dag_circ.qubits().objects().iter().enumerate() {
            let qubit_name = if let Some(locations) = self.dag_circ.qubit_locations().get(qubit) {
                if let Some((register, reg_index)) = locations.registers().first() {
                    format!("{}_{}", register.name(), reg_index)
                } else {
                    format!("q_{}", i)
                }
            } else {
                format!("q_{}", i)
            };
            self.q_wires[i].qubit_name(&qubit_name);
        }
        self.fix_len(" ");
    }

    pub fn build_layer(&mut self, layer: Vec<&PackedInstruction>){
        println!("{:?}",layer);
    }

    pub fn build_layers(&mut self) {
        let binding = self.dag_circ.clone();
        let layer_iterator = binding.multigraph_layers();

        let mut final_layers:Vec<Vec<NodeIndex>> = Vec::new();

        println!("Building layers for the circuit...");

        for (i,layer) in layer_iterator.enumerate(){ 
            
            println!("Processing layer {}", i);

            // NodeIndex is being pushed into each sublayer
            let mut sublayers: Vec<Vec<NodeIndex>> = vec![Vec::new()];
            // let mut sublayers: Vec<Vec<NodeIndex>> = vec![Vec::new(); layer.len()];

            
            for node_index in layer {
                if let NodeType::Operation(instruction_to_insert) = &binding.dag()[node_index] {
                    for sublayer in sublayers.iter_mut() {
                        if sublayer.is_empty() {
                            sublayer.push(node_index);
                        } else {
                            let mut overlap = false;
                            for &sub_node_index in sublayer.iter() {
                                if let NodeType::Operation(instruction) = &binding.dag()[sub_node_index]{
                                    let subnode_qubits = binding.qargs_interner().get(instruction.qubits);
                                    let subnode_clbits = binding.cargs_interner().get(instruction.clbits);
                                    let node_qubits = binding.qargs_interner().get(instruction_to_insert.qubits);
                                    let node_clbits = binding.cargs_interner().get(instruction_to_insert.clbits);

                                    // index can be 0 as well so to unwrap_or with default 0 might not be the best idea
                                    let subnode_min_qubit = subnode_qubits.iter().map(|q| q.0).min().unwrap_or(0);  
                                    let subnode_max_qubit = subnode_qubits.iter().map(|q| q.0).max().unwrap_or(0);
                                    let subnode_min_clbit = subnode_clbits.iter().map(|c| c.0).min().unwrap_or(0);
                                    let subnode_max_clbit = subnode_clbits.iter().map(|c| c.0).max().unwrap_or(0);
                                    let node_min_qubit = node_qubits.iter().map(|q| q.0).min().unwrap_or(0);
                                    let node_max_qubit = node_qubits.iter().map(|q| q.0).max().unwrap_or(0);
                                    let node_min_clbit = node_clbits.iter().map(|c| c.0).min().unwrap_or(0);
                                    let node_max_clbit = node_clbits.iter().map(|c| c.0).max().unwrap_or(0);

                                    // the issue now is that when unwrap_or sets the default to 0 for all instructions without 0
                                    // there will always be overlap
                                    // need to use option to minimise overlap

                                    if (subnode_min_qubit >= node_min_qubit && subnode_min_qubit <= node_max_qubit) ||
                                    (subnode_max_qubit >= node_min_qubit && subnode_max_qubit <= node_max_qubit){
                                        overlap = true;
                                        println!("Conflict detected between subnode {:?} and node {:?}", sub_node_index, node_index);
                                        println!("Subnode qubits: {:?}, Node qubits: {:?}", subnode_qubits, node_qubits);
                                        println!("Subnode clbits: {:?}, Node clbits: {:?}", subnode_clbits, node_clbits);
                                        println!("Subnode min/max qubits: {}, {}, Node min/max qubits: {}, {}", 
                                            subnode_min_qubit, subnode_max_qubit, node_min_qubit, node_max_qubit);
                                        println!("Subnode min/max clbits: {}, {}, Node min/max clbits: {}, {}", 
                                            subnode_min_clbit, subnode_max_clbit, node_min_clbit, node_max_clbit);
                                        break;
                                    }

                                    if (subnode_min_clbit >= node_min_clbit && subnode_min_clbit <= node_max_clbit) ||
                                    (subnode_max_clbit >= node_min_clbit && subnode_max_clbit <= node_max_clbit) {
                                        overlap = true;
                                        println!("Conflict detected between subnode {:?} and node {:?}", sub_node_index, node_index);
                                        println!("Subnode qubits: {:?}, Node qubits: {:?}", subnode_qubits, node_qubits);
                                        println!("Subnode clbits: {:?}, Node clbits: {:?}", subnode_clbits, node_clbits);
                                        println!("Subnode min/max qubits: {}, {}, Node min/max qubits: {}, {}", 
                                            subnode_min_qubit, subnode_max_qubit, node_min_qubit, node_max_qubit);
                                        println!("Subnode min/max clbits: {}, {}, Node min/max clbits: {}, {}", 
                                            subnode_min_clbit, subnode_max_clbit, node_min_clbit, node_max_clbit);
                                        break;
                                    }
                                }
                            }
                            if !overlap {
                                sublayer.push(node_index);
                                break;
                            } else {
                                // If there is a conflict, create a new sublayer
                                let new_sublayer = vec![node_index];
                                sublayers.push(new_sublayer);
                                break;
                            }
                        }
                    }
                    println!("sublayer analyzed: {:?}, instruction: {:?}", sublayers, instruction_to_insert);
                }
            }

            let mut ct = 0;
            for j in sublayers {
                if j.is_empty() {
                    continue;
                } else {
                    final_layers.push(j);
                    ct += 1;
                }
            }
            println!("Layer {} has {} sublayers", i, ct);
        }

        for (i, layer) in final_layers.iter().enumerate() {
            println!("Layer {}: {:?}", i, layer);
        }
        
    }
}

// // print partitions
        // for (i,layer) in layer_iterator.enumerate() {
        //     let mut layer_instructions: Vec<&PackedInstruction> = Vec::new();
        //     println!("Layer: {}",i);
        //     for node_index in layer {
        //         if let NodeType::Operation(instruction) = &binding.dag()[node_index] {
        //             layer_instructions.push(instruction);
        //             //print instruction qubits and clbits
        //             let qubits = binding.qargs_interner().get(instruction.qubits);
        //             let clbits = binding.cargs_interner().get(instruction.clbits);      
        //             println!("  Instruction: {} Qubits: {:?} Clbits: {:?}", instruction.op.name(), qubits, clbits);
        //         }
        //     }
        // }

pub fn circuit_draw(circ_data: &CircuitData) {

    let quantum_circuit_data = QuantumCircuitData {
        data: circ_data.clone(),
        name: None,
        metadata: None,
    };

    let dag_circuit = circuit_to_dag(quantum_circuit_data, true, None, None)
        .expect("Failed to convert circuit data to DAGCircuit");

    let mut output = String::new();

    // Create a circuit representation
    let mut circuit_rep = circuit_rep::new(dag_circuit.clone());
    circuit_rep.set_qubit_name();
    output.push_str(&circuit_rep.circuit_string());
    circuit_rep.build_layers();
    // Print the circuit representation
    println!("{}", output);

    // output.push_str("DAG Circuit Operations:\n");
    // output.push_str(&format!("Number of qubits: {}\n", dag_circuit.num_qubits()));
    // output.push_str(&format!("Number of operations: {}\n", dag_circuit.num_ops()));
    // output.push_str("Operations:\n");
    
    // // creating representation where each wire is represented by 3 strings
    // let mut circuit_rep: Vec<String> = vec![String::new(); (dag_circuit.num_qubits() + 1) * 3];

    // // Fill the first column with qubit labels
    // for (i, qubit) in dag_circuit.qubits().objects().iter().enumerate() {
    //     let qubit_index = i * 3 + 1;
    //     let qubit_name = format!("q_{}: ", i);
    //     circuit_rep[qubit_index].push_str(&qubit_name);
    //     circuit_rep[qubit_index - 1].push_str(" ".repeat((&qubit_name).len()).as_str());
    //     circuit_rep[qubit_index + 1].push_str(" ".repeat((&qubit_name).len()).as_str());
    // }

    // // Print the circuit representation
    // for i in circuit_rep {
    //     println!("{}", i);
    // }
    // //getting qubits and clbit information
    // for (index, qubit) in dag_circuit.qubits().objects().iter().enumerate() {
    //     println!("Qubit {}: {:?}", index, qubit);
    // };

    // // Iterate through clbits with their indices  
    // for (index, clbit) in dag_circuit.clbits().objects().iter().enumerate() {
    //     println!("Clbit {}: {:?}", index, clbit);
    // };

    // let layer_iterator = dag_circuit.multigraph_layers();
    // for (layer_index, layer) in layer_iterator.enumerate() {
    //     output.push_str(&format!("Layer {}:\n", layer_index));
    
    //     // Filter for operation nodes only
    //     let operations: Vec<_> = layer
    //         .into_iter()
    //         .filter_map(|node_index| {
    //             match &dag_circuit.dag()[node_index] {
    //                 NodeType::Operation(instruction) => Some((node_index, instruction)),
    //                 _ => None, // Skip input/output nodes
    //             }
    //         })
    //         .collect();
    
    // if operations.is_empty() {
    //         return;
    //     }

    // for (node_index, instruction) in operations {
    //     let standard_gate = StandardGate::try_from(&instruction.op).unwrap();
    //     let op_name = instruction.op.name();
    //     let qubits = self.dag_circ.qargs_interner().get(instruction.qubits);
    //     let clbits = self.dag_circ.cargs_interner().get(instruction.clbits);

    //     let qubit_str = qubits.iter()
    //         .map(|q| q.0.to_string())
    //         .collect::<Vec<_>>()
    //         .join(",");
        
    //     let clbit_str = clbits.iter()
    //         .map(|c| c.0.to_string())
    //         .collect::<Vec<_>>()
    //         .join(",");
    
    //     // Here, you might want to store or process the operation info as needed.
    //     // For now, just print or log as a placeholder.
    //     println!(
    //         "  Node {}: {} qubits=[{}] clbits=[{}]",
    //         node_index.index(), op_name, qubit_str, clbit_str
    //     );
    
    //     // Print parameters if any
    //     let params = instruction.params_view();
    //     if !params.is_empty() {
    //         println!("    params: {:?}", params);
    //     }
    // }

    // println!("{}",output);
    // }
}
