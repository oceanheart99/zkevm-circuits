//! The Copy circuit implements constraints and lookups for read-write steps for
//! copied bytes while execution opcodes such as CALLDATACOPY, CODECOPY, LOGS,
//! etc.

use bus_mapping::circuit_input_builder::{CopyDataType, CopyEvent, CopyStep, NumberOrHash};

use eth_types::{Field, ToAddress, ToScalar, U256};
use gadgets::{
    binary_number::BinaryNumberChip,
    less_than::{LtChip, LtConfig, LtInstruction},
    util::{and, not, or, Expr},
};
use halo2_proofs::{
    circuit::{Layouter, Region, Value},
    plonk::{Advice, Column, ConstraintSystem, Error, Expression, Fixed, Selector},
    poly::Rotation,
};
use std::iter::once;

use crate::{
    evm_circuit::{
        util::{constraint_builder::BaseConstraintBuilder, rlc, RandomLinearCombination},
        witness::Block,
    },
    table::{
        BytecodeFieldTag, CopyTable, LookupTable, RwTableTag, TxContextFieldTag, TxLogFieldTag,
    },
};

/// Encode the type `NumberOrHash` into a field element
pub fn number_or_hash_to_field<F: Field>(v: &NumberOrHash, randomness: F) -> F {
    match v {
        NumberOrHash::Number(n) => F::from(*n as u64),
        NumberOrHash::Hash(h) => {
            // since code hash in the bytecode table is represented in
            // the little-endian form, we reverse the big-endian bytes
            // of H256.
            let le_bytes = {
                let mut b = h.to_fixed_bytes();
                b.reverse();
                b
            };
            RandomLinearCombination::random_linear_combine(le_bytes, randomness)
        }
    }
}

/// The rw table shared between evm circuit and state circuit
#[derive(Clone, Copy, Debug)]
pub struct CopyCircuit<F> {
    /// Whether the row is enabled or not.
    pub q_enable: Column<Fixed>,
    /// Whether this row denotes a step. A read row is a step and a write row is
    /// not.
    pub q_step: Selector,
    /// Whether the row is the last read-write pair for a copy event.
    pub is_last: Column<Advice>,
    /// The Copy Table contains the columns that are exposed via the lookup
    /// expressions
    pub copy_table: CopyTable,
    /// The value copied in this copy step.
    pub value: Column<Advice>,
    /// In case of a bytecode tag, this denotes whether or not the copied byte
    /// is an opcode or push data byte.
    pub is_code: Column<Advice>,
    /// Whether the row is padding.
    pub is_pad: Column<Advice>,
    /// Lt chip to check: src_addr < src_addr_end.
    /// Since `src_addr` and `src_addr_end` are u64, 8 bytes are sufficient for
    /// the Lt chip.
    pub addr_lt_addr_end: LtConfig<F, 8>,
}

impl<F: Field> CopyCircuit<F> {
    /// Configure the Copy Circuit constraining read-write steps and doing
    /// appropriate lookups to the Tx Table, RW Table and Bytecode Table.
    pub fn configure(
        meta: &mut ConstraintSystem<F>,
        tx_table: &dyn LookupTable<F>,
        rw_table: &dyn LookupTable<F>,
        bytecode_table: &dyn LookupTable<F>,
        copy_table: CopyTable,
        q_enable: Column<Fixed>,
        randomness: Expression<F>,
    ) -> Self {
        let q_step = meta.complex_selector();
        let is_last = meta.advice_column();
        let value = meta.advice_column();
        let is_code = meta.advice_column();
        let is_pad = meta.advice_column();
        let is_first = copy_table.is_first;
        let id = copy_table.id;
        let addr = copy_table.addr;
        let src_addr_end = copy_table.src_addr_end;
        let bytes_left = copy_table.bytes_left;
        let rlc_acc = copy_table.rlc_acc;
        let rw_counter = copy_table.rw_counter;
        let rwc_inc_left = copy_table.rwc_inc_left;
        let tag = copy_table.tag;

        let addr_lt_addr_end = LtChip::configure(
            meta,
            |meta| meta.query_selector(q_step),
            |meta| meta.query_advice(addr, Rotation::cur()),
            |meta| meta.query_advice(src_addr_end, Rotation::cur()),
        );

        meta.create_gate("verify row", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_boolean(
                "is_first is boolean",
                meta.query_advice(is_first, Rotation::cur()),
            );
            cb.require_boolean(
                "is_last is boolean",
                meta.query_advice(is_last, Rotation::cur()),
            );
            cb.require_zero(
                "is_first == 0 when q_step == 0",
                and::expr([
                    not::expr(meta.query_selector(q_step)),
                    meta.query_advice(is_first, Rotation::cur()),
                ]),
            );
            cb.require_zero(
                "is_last == 0 when q_step == 1",
                and::expr([
                    meta.query_advice(is_last, Rotation::cur()),
                    meta.query_selector(q_step),
                ]),
            );

            let not_last_two_rows = 1.expr()
                - meta.query_advice(is_last, Rotation::cur())
                - meta.query_advice(is_last, Rotation::next());
            cb.condition(not_last_two_rows, |cb| {
                cb.require_equal(
                    "rows[0].id == rows[2].id",
                    meta.query_advice(id, Rotation::cur()),
                    meta.query_advice(id, Rotation(2)),
                );
                cb.require_equal(
                    "rows[0].tag == rows[2].tag",
                    tag.value(Rotation::cur())(meta),
                    tag.value(Rotation(2))(meta),
                );
                cb.require_equal(
                    "rows[0].addr + 1 == rows[2].addr",
                    meta.query_advice(addr, Rotation::cur()) + 1.expr(),
                    meta.query_advice(addr, Rotation(2)),
                );
                cb.require_equal(
                    "rows[0].src_addr_end == rows[2].src_addr_end for non-last step",
                    meta.query_advice(src_addr_end, Rotation::cur()),
                    meta.query_advice(src_addr_end, Rotation(2)),
                );
            });

            let rw_diff = and::expr([
                or::expr([
                    tag.value_equals(CopyDataType::Memory, Rotation::cur())(meta),
                    tag.value_equals(CopyDataType::TxLog, Rotation::cur())(meta),
                ]),
                not::expr(meta.query_advice(is_pad, Rotation::cur())),
            ]);
            cb.condition(
                not::expr(meta.query_advice(is_last, Rotation::cur())),
                |cb| {
                    cb.require_equal(
                        "rows[0].rw_counter + rw_diff == rows[1].rw_counter",
                        meta.query_advice(rw_counter, Rotation::cur()) + rw_diff.clone(),
                        meta.query_advice(rw_counter, Rotation::next()),
                    );
                    cb.require_equal(
                        "rows[0].rwc_inc_left - rw_diff == rows[1].rwc_inc_left",
                        meta.query_advice(rwc_inc_left, Rotation::cur()) - rw_diff.clone(),
                        meta.query_advice(rwc_inc_left, Rotation::next()),
                    );
                    cb.require_equal(
                        "rows[0].rlc_acc == rows[1].rlc_acc",
                        meta.query_advice(rlc_acc, Rotation::cur()),
                        meta.query_advice(rlc_acc, Rotation::next()),
                    );
                },
            );
            cb.condition(meta.query_advice(is_last, Rotation::cur()), |cb| {
                cb.require_equal(
                    "rwc_inc_left == rw_diff for last row in the copy slot",
                    meta.query_advice(rwc_inc_left, Rotation::cur()),
                    rw_diff,
                );
            });
            cb.condition(
                and::expr([
                    meta.query_advice(is_last, Rotation::cur()),
                    tag.value_equals(CopyDataType::RlcAcc, Rotation::cur())(meta),
                ]),
                |cb| {
                    cb.require_equal(
                        "value == rlc_acc at the last row for RlcAcc",
                        meta.query_advice(value, Rotation::cur()),
                        meta.query_advice(rlc_acc, Rotation::cur()),
                    );
                },
            );

            cb.gate(meta.query_fixed(q_enable, Rotation::cur()))
        });

        meta.create_gate("verify step (q_step == 1)", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_zero(
                "bytes_left == 1 for last step",
                and::expr([
                    meta.query_advice(is_last, Rotation::next()),
                    1.expr() - meta.query_advice(bytes_left, Rotation::cur()),
                ]),
            );
            cb.condition(
                not::expr(meta.query_advice(is_last, Rotation::next())),
                |cb| {
                    cb.require_equal(
                        "bytes_left == bytes_left_next + 1 for non-last step",
                        meta.query_advice(bytes_left, Rotation::cur()),
                        meta.query_advice(bytes_left, Rotation(2)) + 1.expr(),
                    );
                },
            );
            cb.condition(
                not::expr(tag.value_equals(CopyDataType::RlcAcc, Rotation::next())(
                    meta,
                )),
                |cb| {
                    cb.require_equal(
                        "write value == read value (if not rlc acc)",
                        meta.query_advice(value, Rotation::cur()),
                        meta.query_advice(value, Rotation::next()),
                    );
                },
            );
            cb.condition(meta.query_advice(is_first, Rotation::cur()), |cb| {
                cb.require_equal(
                    "write value == read value (is_first == 1)",
                    meta.query_advice(value, Rotation::cur()),
                    meta.query_advice(value, Rotation::next()),
                );
            });
            cb.require_zero(
                "value == 0 when is_pad == 1 for read",
                and::expr([
                    meta.query_advice(is_pad, Rotation::cur()),
                    meta.query_advice(value, Rotation::cur()),
                ]),
            );
            cb.require_equal(
                "is_pad == 1 - (src_addr < src_addr_end) for read row",
                1.expr() - addr_lt_addr_end.is_lt(meta, None),
                meta.query_advice(is_pad, Rotation::cur()),
            );
            cb.require_zero(
                "is_pad == 0 for write row",
                meta.query_advice(is_pad, Rotation::next()),
            );

            cb.gate(meta.query_selector(q_step))
        });

        meta.create_gate("verify_step (q_step == 0)", |meta| {
            let mut cb = BaseConstraintBuilder::default();

            cb.require_equal(
                "rows[2].value == rows[0].value * r + rows[1].value",
                meta.query_advice(value, Rotation(2)),
                meta.query_advice(value, Rotation::cur()) * randomness
                    + meta.query_advice(value, Rotation::next()),
            );

            cb.gate(and::expr([
                meta.query_fixed(q_enable, Rotation::cur()),
                not::expr(meta.query_selector(q_step)),
                not::expr(meta.query_advice(is_last, Rotation::cur())),
                tag.value_equals(CopyDataType::RlcAcc, Rotation::cur())(meta),
                not::expr(meta.query_advice(is_pad, Rotation::cur())),
            ]))
        });

        meta.lookup_any("Memory lookup", |meta| {
            let cond = meta.query_fixed(q_enable, Rotation::cur())
                * tag.value_equals(CopyDataType::Memory, Rotation::cur())(meta)
                * not::expr(meta.query_advice(is_pad, Rotation::cur()));
            vec![
                meta.query_advice(rw_counter, Rotation::cur()),
                not::expr(meta.query_selector(q_step)),
                RwTableTag::Memory.expr(),
                meta.query_advice(id, Rotation::cur()), // call_id
                meta.query_advice(addr, Rotation::cur()), // memory address
                0.expr(),
                0.expr(),
                meta.query_advice(value, Rotation::cur()),
                0.expr(),
                0.expr(),
                0.expr(),
            ]
            .into_iter()
            .zip(rw_table.table_exprs(meta).into_iter())
            .map(|(arg, table)| (cond.clone() * arg, table))
            .collect()
        });

        meta.lookup_any("TxLog lookup", |meta| {
            let cond = meta.query_fixed(q_enable, Rotation::cur())
                * tag.value_equals(CopyDataType::TxLog, Rotation::cur())(meta);
            vec![
                meta.query_advice(rw_counter, Rotation::cur()),
                1.expr(),
                RwTableTag::TxLog.expr(),
                meta.query_advice(id, Rotation::cur()), // tx_id
                meta.query_advice(addr, Rotation::cur()), // byte_index || field_tag || log_id
                0.expr(),
                0.expr(),
                meta.query_advice(value, Rotation::cur()),
                0.expr(),
                0.expr(),
                0.expr(),
            ]
            .into_iter()
            .zip(rw_table.table_exprs(meta).into_iter())
            .map(|(arg, table)| (cond.clone() * arg, table))
            .collect()
        });

        meta.lookup_any("Bytecode lookup", |meta| {
            let cond = meta.query_fixed(q_enable, Rotation::cur())
                * tag.value_equals(CopyDataType::Bytecode, Rotation::cur())(meta)
                * not::expr(meta.query_advice(is_pad, Rotation::cur()));
            vec![
                meta.query_advice(id, Rotation::cur()),
                BytecodeFieldTag::Byte.expr(),
                meta.query_advice(addr, Rotation::cur()),
                meta.query_advice(is_code, Rotation::cur()),
                meta.query_advice(value, Rotation::cur()),
            ]
            .into_iter()
            .zip(bytecode_table.table_exprs(meta).into_iter())
            .map(|(arg, table)| (cond.clone() * arg, table))
            .collect()
        });

        meta.lookup_any("Tx calldata lookup", |meta| {
            let cond = meta.query_fixed(q_enable, Rotation::cur())
                * tag.value_equals(CopyDataType::TxCalldata, Rotation::cur())(meta)
                * not::expr(meta.query_advice(is_pad, Rotation::cur()));
            vec![
                meta.query_advice(id, Rotation::cur()),
                TxContextFieldTag::CallData.expr(),
                meta.query_advice(addr, Rotation::cur()),
                meta.query_advice(value, Rotation::cur()),
            ]
            .into_iter()
            .zip(tx_table.table_exprs(meta).into_iter())
            .map(|(arg, table)| (cond.clone() * arg, table))
            .collect()
        });

        Self {
            q_enable,
            q_step,
            is_last,
            value,
            is_code,
            is_pad,
            addr_lt_addr_end,
            copy_table,
        }
    }

    /// Assign a witness block to the Copy Circuit.
    pub fn assign_block(
        &self,
        layouter: &mut impl Layouter<F>,
        block: &Block<F>,
        randomness: F,
    ) -> Result<(), Error> {
        let tag_chip = BinaryNumberChip::construct(self.copy_table.tag);
        let lt_chip = LtChip::construct(self.addr_lt_addr_end);

        layouter.assign_region(
            || "assign copy table",
            |mut region| {
                let mut offset = 0;
                for copy_event in block.copy_events.iter() {
                    let rlc_acc = if copy_event.dst_type == CopyDataType::RlcAcc {
                        let values = copy_event
                            .bytes
                            .iter()
                            .map(|(value, _is_code)| *value)
                            .collect::<Vec<u8>>();
                        rlc::value(values.iter().rev(), randomness)
                    } else {
                        F::zero()
                    };
                    let mut value_acc = F::zero();
                    for (step_idx, (is_read_step, copy_step)) in copy_event
                        .bytes
                        .iter()
                        .flat_map(|(value, is_code)| {
                            let read_step = CopyStep {
                                value: *value,
                                is_code: if copy_event.src_type == CopyDataType::Bytecode {
                                    Some(*is_code)
                                } else {
                                    None
                                },
                            };
                            let write_step = CopyStep {
                                value: *value,
                                is_code: if copy_event.dst_type == CopyDataType::Bytecode {
                                    Some(*is_code)
                                } else {
                                    None
                                },
                            };
                            once((true, read_step)).chain(once((false, write_step)))
                        })
                        .enumerate()
                    {
                        let value = if copy_event.dst_type == CopyDataType::RlcAcc {
                            if is_read_step {
                                F::from(copy_step.value as u64)
                            } else {
                                value_acc =
                                    value_acc * randomness + F::from(copy_step.value as u64);
                                value_acc
                            }
                        } else {
                            F::from(copy_step.value as u64)
                        };
                        self.assign_step(
                            &mut region,
                            offset,
                            randomness,
                            copy_event,
                            step_idx,
                            &copy_step,
                            value,
                            rlc_acc,
                            &tag_chip,
                            &lt_chip,
                        )?;
                        offset += 1;
                    }
                }
                // pad two rows in the end to satisfy Halo2 cell assignment check
                for _ in 0..2 {
                    self.assign_padding_row(&mut region, offset, &tag_chip)?;
                    offset += 1;
                }
                Ok(())
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn assign_step(
        &self,
        region: &mut Region<F>,
        offset: usize,
        randomness: F,
        copy_event: &CopyEvent,
        step_idx: usize,
        copy_step: &CopyStep,
        value: F,
        rlc_acc: F,
        tag_chip: &BinaryNumberChip<F, CopyDataType, 3>,
        lt_chip: &LtChip<F, 8>,
    ) -> Result<(), Error> {
        // q_enable
        region.assign_fixed(
            || "q_enable",
            self.q_enable,
            offset,
            || Value::known(F::one()),
        )?;
        // enable q_step on the Read step
        let is_read = step_idx % 2 == 0;
        if is_read {
            self.q_step.enable(region, offset)?;
        }

        let id = if is_read {
            &copy_event.src_id
        } else {
            &copy_event.dst_id
        };
        let bytes_left = u64::try_from(copy_event.bytes.len() - step_idx / 2).unwrap();

        // is_first
        region.assign_advice(
            || format!("assign is_first {}", offset),
            self.copy_table.is_first,
            offset,
            || Value::known(if step_idx == 0 { F::one() } else { F::zero() }),
        )?;
        // is_last
        region.assign_advice(
            || format!("assign is_last {}", offset),
            self.is_last,
            offset,
            || {
                Value::known(if step_idx == copy_event.bytes.len() * 2 - 1 {
                    F::one()
                } else {
                    F::zero()
                })
            },
        )?;
        // id
        region.assign_advice(
            || format!("assign id {}", offset),
            self.copy_table.id,
            offset,
            || Value::known(number_or_hash_to_field(id, randomness)),
        )?;
        // addr
        let copy_step_addr: u64 =
            if is_read {
                copy_event.src_addr
            } else {
                copy_event.dst_addr
            } + (u64::try_from(step_idx).unwrap() - if is_read { 0 } else { 1 }) / 2u64;

        let addr = if is_read && copy_event.dst_type == CopyDataType::TxLog {
            (U256::from(copy_step_addr)
                + (U256::from(TxLogFieldTag::Data as u64) << 32)
                + (U256::from(copy_event.log_id.unwrap()) << 48))
                .to_address()
                .to_scalar()
                .unwrap()
        } else {
            F::from(copy_step_addr)
        };
        region.assign_advice(
            || format!("assign addr {}", offset),
            self.copy_table.addr,
            offset,
            || Value::known(addr),
        )?;
        // value
        region.assign_advice(
            || format!("assign value {}", offset),
            self.value,
            offset,
            || Value::known(value),
        )?;
        // rlc_acc
        region.assign_advice(
            || format!("assign rlc_acc {}", offset),
            self.copy_table.rlc_acc,
            offset,
            || Value::known(rlc_acc),
        )?;
        // is_code
        region.assign_advice(
            || format!("assign is_code {}", offset),
            self.is_code,
            offset,
            || Value::known(copy_step.is_code.map_or(F::zero(), |v| F::from(v))),
        )?;
        // is_pad
        let is_pad = is_read && copy_step_addr >= copy_event.src_addr_end;
        region.assign_advice(
            || format!("assign is_pad {}", offset),
            self.is_pad,
            offset,
            || Value::known(F::from(is_pad)),
        )?;
        // rw_counter
        region.assign_advice(
            || format!("assign rw_counter {}", offset),
            self.copy_table.rw_counter,
            offset,
            || Value::known(F::from(copy_event.rw_counter(step_idx))),
        )?;
        // rwc_inc_left
        region.assign_advice(
            || format!("assign rwc_inc_left {}", offset),
            self.copy_table.rwc_inc_left,
            offset,
            || Value::known(F::from(copy_event.rw_counter_increase_left(step_idx))),
        )?;
        // tag binary number chip
        let tag = if is_read {
            copy_event.src_type
        } else {
            copy_event.dst_type
        };
        tag_chip.assign(region, offset, &tag)?;
        // assignment for read steps
        if is_read {
            // src_addr_end
            region.assign_advice(
                || format!("assign src_addr_end {}", offset),
                self.copy_table.src_addr_end,
                offset,
                || Value::known(F::from(copy_event.src_addr_end)),
            )?;
            // bytes_left
            region.assign_advice(
                || format!("assign bytes_left {}", offset),
                self.copy_table.bytes_left,
                offset,
                || Value::known(F::from(bytes_left)),
            )?;
            // lt chip
            lt_chip.assign(
                region,
                offset,
                F::from(copy_step_addr),
                F::from(copy_event.src_addr_end),
            )?;
        }
        Ok(())
    }

    fn assign_padding_row(
        &self,
        region: &mut Region<F>,
        offset: usize,
        tag_chip: &BinaryNumberChip<F, CopyDataType, 3>,
    ) -> Result<(), Error> {
        // q_enable
        region.assign_fixed(
            || "q_enable",
            self.q_enable,
            offset,
            || Value::known(F::zero()),
        )?;
        // is_first
        region.assign_advice(
            || format!("assign is_first {}", offset),
            self.copy_table.is_first,
            offset,
            || Value::known(F::zero()),
        )?;
        // is_last
        region.assign_advice(
            || format!("assign is_last {}", offset),
            self.is_last,
            offset,
            || Value::known(F::zero()),
        )?;
        // id
        region.assign_advice(
            || format!("assign id {}", offset),
            self.copy_table.id,
            offset,
            || Value::known(F::zero()),
        )?;
        // addr
        region.assign_advice(
            || format!("assign addr {}", offset),
            self.copy_table.addr,
            offset,
            || Value::known(F::zero()),
        )?;
        // src_addr_end
        region.assign_advice(
            || format!("assign src_addr_end {}", offset),
            self.copy_table.src_addr_end,
            offset,
            || Value::known(F::zero()),
        )?;
        // bytes_left
        region.assign_advice(
            || format!("assign bytes_left {}", offset),
            self.copy_table.bytes_left,
            offset,
            || Value::known(F::zero()),
        )?;
        // value
        region.assign_advice(
            || format!("assign value {}", offset),
            self.value,
            offset,
            || Value::known(F::zero()),
        )?;
        // rlc_acc
        region.assign_advice(
            || format!("assign rlc_acc {}", offset),
            self.copy_table.rlc_acc,
            offset,
            || Value::known(F::zero()),
        )?;
        // is_code
        region.assign_advice(
            || format!("assign is_code {}", offset),
            self.is_code,
            offset,
            || Value::known(F::zero()),
        )?;
        // is_pad
        region.assign_advice(
            || format!("assign is_pad {}", offset),
            self.is_pad,
            offset,
            || Value::known(F::zero()),
        )?;
        // rw_counter
        region.assign_advice(
            || format!("assign rw_counter {}", offset),
            self.copy_table.rw_counter,
            offset,
            || Value::known(F::zero()),
        )?;
        // rwc_inc_left
        region.assign_advice(
            || format!("assign rwc_inc_left {}", offset),
            self.copy_table.rwc_inc_left,
            offset,
            || Value::known(F::zero()),
        )?;
        // tag
        tag_chip.assign(region, offset, &CopyDataType::default())?;
        Ok(())
    }
}

/// Dev helpers
#[cfg(any(feature = "test", test))]
pub mod dev {
    use super::*;
    use eth_types::Field;
    use halo2_proofs::{
        circuit::{Layouter, SimpleFloorPlanner},
        dev::{MockProver, VerifyFailure},
        plonk::{Circuit, ConstraintSystem},
    };

    use crate::{
        evm_circuit::witness::Block,
        table::{BytecodeTable, RwTable, TxTable},
        util::{power_of_randomness_from_instance, Challenges},
    };

    #[derive(Clone)]
    struct CopyCircuitTesterConfig<F> {
        tx_table: TxTable,
        rw_table: RwTable,
        bytecode_table: BytecodeTable,
        copy_circuit: CopyCircuit<F>,
    }

    #[derive(Default)]
    struct CopyCircuitTester<F> {
        block: Block<F>,
        randomness: F,
    }

    impl<F: Field> CopyCircuitTester<F> {
        fn get_randomness() -> F {
            F::random(rand::thread_rng())
        }

        pub fn new(block: Block<F>, randomness: F) -> Self {
            Self { block, randomness }
        }
        pub fn r() -> Expression<F> {
            123456u64.expr()
        }
    }

    impl<F: Field> Circuit<F> for CopyCircuitTester<F> {
        type Config = CopyCircuitTesterConfig<F>;
        type FloorPlanner = SimpleFloorPlanner;

        fn without_witnesses(&self) -> Self {
            Self::default()
        }

        fn configure(meta: &mut ConstraintSystem<F>) -> Self::Config {
            let tx_table = TxTable::construct(meta);
            let rw_table = RwTable::construct(meta);
            let bytecode_table = BytecodeTable::construct(meta);
            let q_enable = meta.fixed_column();

            let randomness = power_of_randomness_from_instance::<_, 1>(meta);
            let copy_table = CopyTable::construct(meta, q_enable);
            let copy_circuit = CopyCircuit::configure(
                meta,
                &tx_table,
                &rw_table,
                &bytecode_table,
                copy_table,
                q_enable,
                randomness[0].clone(),
            );

            CopyCircuitTesterConfig {
                tx_table,
                rw_table,
                bytecode_table,
                copy_circuit,
            }
        }

        fn synthesize(
            &self,
            config: Self::Config,
            mut layouter: impl Layouter<F>,
        ) -> Result<(), halo2_proofs::plonk::Error> {
            let challenges = Challenges::mock(Value::known(self.randomness));

            config
                .tx_table
                .load(&mut layouter, &self.block.txs, self.randomness)?;
            config.rw_table.load(
                &mut layouter,
                &self.block.rws.table_assignments(),
                self.block.state_circuit_pad_to,
                self.randomness,
            )?;
            config.bytecode_table.load(
                &mut layouter,
                self.block.bytecodes.values(),
                &challenges,
            )?;
            config
                .copy_circuit
                .assign_block(&mut layouter, &self.block, self.randomness)
        }
    }

    /// Test copy circuit with the provided block witness
    pub fn test_copy_circuit<F: Field>(k: u32, block: Block<F>) -> Result<(), Vec<VerifyFailure>> {
        let randomness = CopyCircuitTester::<F>::get_randomness();
        let circuit = CopyCircuitTester::<F>::new(block, randomness);
        let num_rows = 1 << k;
        const NUM_BLINDING_ROWS: usize = 7 - 1;
        let instance = vec![vec![randomness; num_rows - NUM_BLINDING_ROWS]];
        let prover = MockProver::<F>::run(k, &circuit, instance).unwrap();
        prover.verify()
    }
}

#[cfg(test)]
mod tests {
    use super::dev::test_copy_circuit;
    use bus_mapping::evm::{gen_sha3_code, MemoryKind};
    use bus_mapping::{circuit_input_builder::CircuitInputBuilder, mock::BlockData};
    use eth_types::{bytecode, geth_types::GethData, Word};
    use mock::test_ctx::helpers::account_0_code_account_1_no_code;
    use mock::TestContext;

    use crate::evm_circuit::test::rand_bytes;
    use crate::evm_circuit::witness::block_convert;

    fn gen_calldatacopy_data() -> CircuitInputBuilder {
        let length = 0x0fffusize;
        let code = bytecode! {
            PUSH32(Word::from(length))
            PUSH32(Word::from(0x00))
            PUSH32(Word::from(0x00))
            CALLDATACOPY
            STOP
        };
        let calldata = rand_bytes(length);
        let test_ctx = TestContext::<2, 1>::new(
            None,
            account_0_code_account_1_no_code(code),
            |mut txs, accs| {
                txs[0]
                    .from(accs[1].address)
                    .to(accs[0].address)
                    .input(calldata.into());
            },
            |block, _txs| block.number(0xcafeu64),
        )
        .unwrap();
        let block: GethData = test_ctx.into();
        let mut builder = BlockData::new_from_geth_data(block.clone()).new_circuit_input_builder();
        builder
            .handle_block(&block.eth_block, &block.geth_traces)
            .unwrap();
        builder
    }

    fn gen_codecopy_data() -> CircuitInputBuilder {
        let code = bytecode! {
            PUSH32(Word::from(0x20))
            PUSH32(Word::from(0x00))
            PUSH32(Word::from(0x00))
            CODECOPY
            STOP
        };
        let test_ctx = TestContext::<2, 1>::simple_ctx_with_bytecode(code).unwrap();
        let block: GethData = test_ctx.into();
        let mut builder = BlockData::new_from_geth_data(block.clone()).new_circuit_input_builder();
        builder
            .handle_block(&block.eth_block, &block.geth_traces)
            .unwrap();
        builder
    }

    fn gen_sha3_data() -> CircuitInputBuilder {
        let (code, _) = gen_sha3_code(0x20, 0x200, MemoryKind::EqualToSize);
        let test_ctx = TestContext::<2, 1>::simple_ctx_with_bytecode(code).unwrap();
        let block: GethData = test_ctx.into();
        let mut builder = BlockData::new_from_geth_data(block.clone()).new_circuit_input_builder();
        builder
            .handle_block(&block.eth_block, &block.geth_traces)
            .unwrap();
        builder
    }

    #[test]
    fn copy_circuit_valid_calldatacopy() {
        let builder = gen_calldatacopy_data();
        let block = block_convert(&builder.block, &builder.code_db);
        assert_eq!(test_copy_circuit(14, block), Ok(()));
    }

    #[test]
    fn copy_circuit_valid_codecopy() {
        let builder = gen_codecopy_data();
        let block = block_convert(&builder.block, &builder.code_db);
        assert_eq!(test_copy_circuit(10, block), Ok(()));
    }

    #[test]
    fn copy_circuit_valid_sha3() {
        let builder = gen_sha3_data();
        let block = block_convert(&builder.block, &builder.code_db);
        assert_eq!(test_copy_circuit(20, block), Ok(()));
    }

    // // TODO: replace these with deterministic failure tests
    // fn perturb_tag(block: &mut bus_mapping::circuit_input_builder::Block) {
    //     debug_assert!(!block.copy_events.is_empty());
    //     debug_assert!(!block.copy_events[0].steps.is_empty());
    //
    //     let copy_event = &mut block.copy_events[0];
    //     let mut rng = rand::thread_rng();
    //     let rand_idx = (0..copy_event.steps.len()).choose(&mut rng).unwrap();
    //     let (is_read_step, mut perturbed_step) = match rng.gen::<f32>() {
    //         f if f < 0.5 => (true, copy_event.steps[rand_idx].0.clone()),
    //         _ => (false, copy_event.steps[rand_idx].1.clone()),
    //     };
    //     match rng.gen::<f32>() {
    //         _ => perturbed_step.value = rng.gen(),
    //     }
    //
    //         copy_event.bytes[rand_idx] = perturbed_step;
    // }

    // #[test]
    // fn copy_circuit_invalid_calldatacopy() {
    //     let mut builder = gen_calldatacopy_data();
    //     perturb_tag(&mut builder.block);
    //     let block = block_convert(&builder.block, &builder.code_db);
    //     assert!(test_copy_circuit(10, block).is_err());
    // }

    // #[test]
    // fn copy_circuit_invalid_codecopy() {
    //     let mut builder = gen_codecopy_data();
    //     perturb_tag(&mut builder.block);
    //     let block = block_convert(&builder.block, &builder.code_db);
    //     assert!(test_copy_circuit(10, block).is_err());
    // }

    // #[test]
    // fn copy_circuit_invalid_sha3() {
    //     let mut builder = gen_sha3_data();
    //     perturb_tag(&mut builder.block);
    //     let block = block_convert(&builder.block, &builder.code_db);
    //     assert!(test_copy_circuit(20, block).is_err());
    // }
}
