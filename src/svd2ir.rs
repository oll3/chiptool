use log::*;
use std::collections::{HashMap, HashSet};
use svd_parser as svd;

use crate::util;
use crate::{ir::*, transform};

#[derive(Debug)]
struct ProtoBlock {
    name: Vec<String>,
    description: Option<String>,
    registers: Vec<svd::RegisterCluster>,
}

#[derive(Debug)]
struct ProtoFieldset {
    name: Vec<String>,
    description: Option<String>,
    bit_size: u32,
    fields: Vec<svd::Field>,
}

#[derive(Debug)]
struct ProtoEnum {
    name: Vec<String>,
    bit_size: u32,
    variants: Vec<svd::EnumeratedValue>,
}

pub fn convert_peripheral(ir: &mut IR, p: &svd::Peripheral) -> anyhow::Result<()> {
    let mut blocks = Vec::new();
    collect_blocks(
        &mut blocks,
        vec![p.name.clone()],
        p.description.clone(),
        p.registers.as_ref().unwrap(),
    );

    let mut fieldsets: Vec<ProtoFieldset> = Vec::new();
    let mut enums: Vec<ProtoEnum> = Vec::new();

    for block in &blocks {
        for r in &block.registers {
            if let svd::RegisterCluster::Register(r) = r {
                if r.derived_from.is_some() {
                    continue;
                }

                if let Some(fields) = &r.fields {
                    let mut fieldset_name = block.name.clone();
                    fieldset_name.push(util::replace_suffix(&r.name, ""));
                    fieldsets.push(ProtoFieldset {
                        name: fieldset_name.clone(),
                        description: r.description.clone(),
                        bit_size: 32, // todo
                        fields: fields.clone(),
                    });

                    for f in fields {
                        if f.derived_from.is_some() {
                            continue;
                        }

                        let mut enum_read = None;
                        let mut enum_write = None;
                        let mut enum_readwrite = None;

                        for e in &f.enumerated_values {
                            if e.derived_from.is_some() {
                                // TODO
                                warn!("ignoring enum with derivedFrom");
                                continue;
                            }

                            let usage = e.usage.unwrap_or(svd::Usage::ReadWrite);
                            let target = match usage {
                                svd::Usage::Read => &mut enum_read,
                                svd::Usage::Write => &mut enum_write,
                                svd::Usage::ReadWrite => &mut enum_readwrite,
                            };

                            if target.is_some() {
                                warn!("ignoring enum with dup usage {:?}", usage);
                                continue;
                            }

                            *target = Some(e)
                        }

                        enum EnumSet<'a> {
                            Single(&'a svd::EnumeratedValues),
                            ReadWrite(&'a svd::EnumeratedValues, &'a svd::EnumeratedValues),
                        }

                        let set = match (enum_read, enum_write, enum_readwrite) {
                            (None, None, None) => None,
                            (Some(e), None, None) => Some(EnumSet::Single(e)),
                            (None, Some(e), None) => Some(EnumSet::Single(e)),
                            (None, None, Some(e)) => Some(EnumSet::Single(e)),
                            (Some(r), Some(w), None) => Some(EnumSet::ReadWrite(r, w)),
                            (Some(r), None, Some(w)) => Some(EnumSet::ReadWrite(r, w)),
                            (None, Some(w), Some(r)) => Some(EnumSet::ReadWrite(r, w)),
                            (Some(_), Some(_), Some(_)) => panic!(
                                "cannot have enumeratedvalues for read, write and readwrite!"
                            ),
                        };

                        if let Some(set) = set {
                            let variants = match set {
                                EnumSet::Single(e) => e.values.clone(),
                                EnumSet::ReadWrite(r, w) => {
                                    let r_values = r.values.iter().map(|v| v.value.unwrap());
                                    let w_values = w.values.iter().map(|v| v.value.unwrap());
                                    let values: HashSet<_> = r_values.chain(w_values).collect();
                                    let mut values: Vec<_> = values.iter().collect();
                                    values.sort();

                                    let r_values: HashMap<_, _> =
                                        r.values.iter().map(|v| (v.value.unwrap(), v)).collect();
                                    let w_values: HashMap<_, _> =
                                        w.values.iter().map(|v| (v.value.unwrap(), v)).collect();

                                    values
                                        .into_iter()
                                        .map(|&v| match (r_values.get(&v), w_values.get(&v)) {
                                            (None, None) => unreachable!(),
                                            (Some(&r), None) => r.clone(),
                                            (None, Some(&w)) => w.clone(),
                                            (Some(&r), Some(&w)) => {
                                                let mut m = r.clone();
                                                if r.name != w.name {
                                                    m.name = format!("R_{}_W_{}", r.name, w.name);
                                                }
                                                m
                                            }
                                        })
                                        .collect()
                                }
                            };

                            let mut name = fieldset_name.clone();
                            name.push(f.name.clone());
                            enums.push(ProtoEnum {
                                name,
                                bit_size: f.bit_range.width,
                                variants,
                            });
                        }
                    }
                };
            }
        }
    }

    // Make all collected names unique by prefixing with parents' names if needed.
    let block_names = unique_names(blocks.iter().map(|x| x.name.clone()).collect());
    let fieldset_names = unique_names(fieldsets.iter().map(|x| x.name.clone()).collect());
    let enum_names = unique_names(enums.iter().map(|x| x.name.clone()).collect());

    // Convert blocks
    for proto in &blocks {
        let mut block = Block {
            extends: None,
            description: proto.description.clone(),
            items: Vec::new(),
        };

        for r in &proto.registers {
            match r {
                svd::RegisterCluster::Register(r) => {
                    if r.derived_from.is_some() {
                        warn!("unsupported derived_from in registers");
                        continue;
                    }

                    let fieldset_name = if r.fields.is_some() {
                        let mut fieldset_name = proto.name.clone();
                        fieldset_name.push(util::replace_suffix(&r.name, ""));
                        Some(fieldset_names.get(&fieldset_name).unwrap().clone())
                    } else {
                        None
                    };

                    let array = if let svd::Register::Array(_, dim) = r {
                        Some(Array::Regular(RegularArray {
                            len: dim.dim,
                            stride: dim.dim_increment,
                        }))
                    } else {
                        None
                    };

                    let access = match r.access {
                        None => Access::ReadWrite,
                        Some(svd::Access::ReadOnly) => Access::Read,
                        Some(svd::Access::WriteOnly) => Access::Write,
                        Some(svd::Access::WriteOnce) => Access::Write,
                        Some(svd::Access::ReadWrite) => Access::ReadWrite,
                        Some(svd::Access::ReadWriteOnce) => Access::ReadWrite,
                    };

                    let block_item = BlockItem {
                        name: util::replace_suffix(&r.name, ""),
                        description: r.description.clone(),
                        array,
                        byte_offset: r.address_offset,
                        inner: BlockItemInner::Register(Register {
                            access, // todo
                            bit_size: r.size.unwrap_or(32),
                            fieldset: fieldset_name.clone(),
                        }),
                    };

                    block.items.push(block_item)
                }
                svd::RegisterCluster::Cluster(c) => {
                    if c.derived_from.is_some() {
                        warn!("unsupported derived_from in clusters");
                        continue;
                    }

                    let cname = util::replace_suffix(&c.name, "");

                    let array = if let svd::Cluster::Array(_, dim) = c {
                        Some(Array::Regular(RegularArray {
                            len: dim.dim,
                            stride: dim.dim_increment,
                        }))
                    } else {
                        None
                    };

                    let mut block_name = proto.name.clone();
                    block_name.push(util::replace_suffix(&c.name, ""));
                    let block_name = block_names.get(&block_name).unwrap().clone();

                    block.items.push(BlockItem {
                        name: cname.clone(),
                        description: c.description.clone(),
                        array,
                        byte_offset: c.address_offset,
                        inner: BlockItemInner::Block(BlockItemBlock { block: block_name }),
                    });
                }
            }
        }

        let block_name = block_names.get(&proto.name).unwrap().clone();
        assert!(ir.blocks.insert(block_name, block).is_none())
    }

    // Convert fieldsets
    for proto in &fieldsets {
        let mut fieldset = FieldSet {
            extends: None,
            description: proto.description.clone(),
            bit_size: proto.bit_size,
            fields: Vec::new(),
        };

        for f in &proto.fields {
            if f.derived_from.is_some() {
                warn!("unsupported derived_from in fieldset");
            }

            let mut field = Field {
                name: f.name.clone(),
                description: f.description.clone(),
                bit_offset: f.bit_range.offset,
                bit_size: f.bit_range.width,
                array: None,
                enumm: None,
            };

            if !f.enumerated_values.is_empty() {
                let mut enum_name = proto.name.clone();
                enum_name.push(f.name.clone());

                trace!("finding enum {:?}", enum_name);
                let enum_name = enum_names.get(&enum_name).unwrap().clone();
                trace!("found {:?}", enum_name);
                field.enumm = Some(enum_name.clone())
            }

            fieldset.fields.push(field)
        }

        let fieldset_name = fieldset_names.get(&proto.name).unwrap().clone();
        assert!(ir.fieldsets.insert(fieldset_name, fieldset).is_none())
    }

    for proto in &enums {
        let variants = proto
            .variants
            .iter()
            .map(|v| EnumVariant {
                description: v.description.clone(),
                name: v.name.clone(),
                value: v.value.unwrap() as _, // TODO what are variants without values used for??
            })
            .collect();

        let enumm = Enum {
            description: None,
            bit_size: proto.bit_size,
            variants,
        };

        let enum_name = enum_names.get(&proto.name).unwrap().clone();
        assert!(ir.enums.insert(enum_name.clone(), enumm).is_none());
    }

    Ok(())
}

pub fn convert_svd(svd: &svd::Device) -> anyhow::Result<IR> {
    let mut ir = IR::new();

    let mut device = Device {
        nvic_priority_bits: svd.cpu.as_ref().map(|cpu| cpu.nvic_priority_bits as u8),
        peripherals: vec![],
        interrupts: vec![],
    };

    for p in &svd.peripherals {
        let block_name = p.derived_from.as_ref().unwrap_or(&p.name);
        let block_name = format!("{}::{}", block_name, block_name);
        let periname = p.name.to_ascii_uppercase();

        let peri = Peripheral {
            name: periname.clone(),
            description: p.description.clone(),
            base_address: p.base_address,
            block: Some(block_name),
            array: None,
            interrupts: HashMap::new(),
        };

        let mut irqs: Vec<&svd::Interrupt> = vec![];
        for i in &p.interrupt {
            if !irqs.iter().any(|&j| j.name == i.name) {
                irqs.push(i)
            }
        }
        irqs.sort_by_key(|i| &i.name);

        for (_n, &i) in irqs.iter().enumerate() {
            let iname = i.name.to_ascii_uppercase();

            if !device.interrupts.iter().any(|j| j.name == iname) {
                device.interrupts.push(Interrupt {
                    name: iname.clone(),
                    description: i.description.clone(),
                    value: i.value,
                });
            }

            /*
            let name = if iname.len() > periname.len() && iname.starts_with(&periname) {
                let s = iname.strip_prefix(&periname).unwrap();
                s.trim_matches('_').to_string()
            } else if irqs.len() == 1 {
                "IRQ".to_string()
            } else {
                format!("IRQ{}", n)
            };

            peri.interrupts.insert(name, iname.clone());
             */
        }

        device.peripherals.push(peri);

        if p.derived_from.is_none() {
            let mut pir = IR::new();
            convert_peripheral(&mut pir, p)?;

            let path = &p.name;
            transform::map_names(&mut pir, |k, s| match k {
                transform::NameKind::Block => *s = format!("{}::{}", path, s),
                transform::NameKind::Fieldset => *s = format!("{}::regs::{}", path, s),
                transform::NameKind::Enum => *s = format!("{}::vals::{}", path, s),
                _ => {}
            });

            ir.merge(pir);
        }
    }

    ir.devices.insert("".to_string(), device);

    transform::sort::Sort {}.run(&mut ir).unwrap();
    transform::Sanitize {}.run(&mut ir).unwrap();

    Ok(ir)
}

fn collect_blocks(
    out: &mut Vec<ProtoBlock>,
    block_name: Vec<String>,
    description: Option<String>,
    registers: &[svd::RegisterCluster],
) {
    out.push(ProtoBlock {
        name: block_name.clone(),
        description,
        registers: registers.to_owned(),
    });

    for r in registers {
        if let svd::RegisterCluster::Cluster(c) = r {
            if c.derived_from.is_some() {
                continue;
            }

            let mut block_name = block_name.clone();
            block_name.push(util::replace_suffix(&c.name, ""));
            collect_blocks(out, block_name, c.description.clone(), &c.children);
        }
    }
}

fn unique_names(names: Vec<Vec<String>>) -> HashMap<Vec<String>, String> {
    let mut res = HashMap::new();
    let mut seen = HashSet::new();

    let suffix_exists = |n: &[String], i: usize| {
        names
            .iter()
            .enumerate()
            .filter(|(j, _)| *j != i)
            .any(|(_, n2)| n2.ends_with(n))
    };
    for (i, n) in names.iter().enumerate() {
        let j = (0..n.len())
            .rev()
            .find(|&j| !suffix_exists(&n[j..], i))
            .or_else(|| (0..n.len()).rev().find(|&j| !seen.contains(&n[j..])))
            .unwrap();
        assert!(res.insert(n.clone(), n[j..].join("_")).is_none());
        seen.insert(&n[j..]);
    }
    res
}
