use rspirv::binary::Assemble;
use rspirv::binary::Consumer;
use rspirv::binary::Disassemble;
use rspirv::spirv;
use std::collections::{HashMap, HashSet};

fn load(bytes: &[u8]) -> rspirv::dr::Module {
    let mut loader = rspirv::dr::Loader::new();
    rspirv::binary::parse_bytes(&bytes, &mut loader).unwrap();
    let module = loader.module();
    module
}

fn shift_ids(module: &mut rspirv::dr::Module, add: u32) {
    module.all_inst_iter_mut().for_each(|inst| {
        if let Some(ref mut result_id) = &mut inst.result_id {
            *result_id += add;
        }

        if let Some(ref mut result_type) = &mut inst.result_type {
            *result_type += add;
        }

        inst.operands.iter_mut().for_each(|op| match op {
            rspirv::dr::Operand::IdMemorySemantics(w)
            | rspirv::dr::Operand::IdScope(w)
            | rspirv::dr::Operand::IdRef(w) => *w += add,
            _ => {}
        })
    });
}

fn replace_all_uses_with(module: &mut rspirv::dr::Module, before: u32, after: u32) {
    module.all_inst_iter_mut().for_each(|inst| {
        if let Some(ref mut result_type) = &mut inst.result_type {
            if *result_type == before {
                *result_type = after;
            }
        }

        inst.operands.iter_mut().for_each(|op| match op {
            rspirv::dr::Operand::IdMemorySemantics(w)
            | rspirv::dr::Operand::IdScope(w)
            | rspirv::dr::Operand::IdRef(w) => {
                if *w == before {
                    *w = after
                }
            }
            _ => {}
        })
    });
}

fn remove_duplicate_capablities(module: &mut rspirv::dr::Module) {
    let mut set = HashSet::new();
    let mut caps = vec![];

    for c in &module.capabilities {
        let keep = match c.operands[0] {
            rspirv::dr::Operand::Capability(cap) => set.insert(cap),
            _ => true,
        };

        if keep {
            caps.push(c.clone());
        }
    }

    module.capabilities = caps;
}

fn remove_duplicate_ext_inst_imports(module: &mut rspirv::dr::Module) {
    let mut set = HashSet::new();
    let mut caps = vec![];

    for c in &module.ext_inst_imports {
        let keep = match &c.operands[0] {
            rspirv::dr::Operand::LiteralString(ext_inst_import) => set.insert(ext_inst_import),
            _ => true,
        };

        if keep {
            caps.push(c.clone());
        }
    }

    module.ext_inst_imports = caps;
}

fn kill_with_id(insts: &mut Vec<rspirv::dr::Instruction>, id: u32) {
    let mut idx = insts.len() - 1;
    // odd backwards loop so we can swap_remove
    loop {
        match insts[idx].operands[0] {
            rspirv::dr::Operand::IdMemorySemantics(w)
            | rspirv::dr::Operand::IdScope(w)
            | rspirv::dr::Operand::IdRef(w) => {
                if w == id {
                    insts.swap_remove(idx);
                }
            }
            _ => {}
        }

        if idx == 0 {
            break;
        }

        idx -= 1;
    }
}

fn kill_annotations_and_debug(module: &mut rspirv::dr::Module, id: u32) {
    kill_with_id(&mut module.annotations, id);
    kill_with_id(&mut module.debugs, id);
}

fn remove_duplicate_types(module: &mut rspirv::dr::Module) {
    // jb-todo: spirv-tools's linker has special case handling for SpvOpTypeForwardPointer,
    // not sure if we need that; see https://github.com/KhronosGroup/SPIRV-Tools/blob/e7866de4b1dc2a7e8672867caeb0bdca49f458d3/source/opt/remove_duplicates_pass.cpp for reference
    let mut start = 0;

    // need to do this process iteratively because types can reference each other
    loop {
        let mut replace = None;

        // start with `nth` so we can restart this loop quickly after killing the op
        for (i_idx, i) in module.types_global_values.iter().enumerate().nth(start) {
            let mut identical = None;
            for j in module.types_global_values.iter().skip(i_idx + 1) {
                if i.is_type_identical(j) {
                    identical = j.result_id;
                    break;
                }
            }

            if let Some(identical) = identical {
                replace = Some((i.result_id.unwrap(), identical, i_idx));
                break;
            }
        }

        // can't do this directly in the previous loop because of the
        // mut borrow needed on `module`
        if let Some((remove, keep, kill_idx)) = replace {
            kill_annotations_and_debug(module, remove);
            replace_all_uses_with(module, remove, keep);
            module.types_global_values.swap_remove(kill_idx);
            start = kill_idx; // jb-todo: is it correct to restart this loop here?
        } else {
            break;
        }
    }
}

fn remove_duplicates(module: &mut rspirv::dr::Module) {
    remove_duplicate_capablities(module);
    remove_duplicate_ext_inst_imports(module);
    remove_duplicate_types(module);
    // jb-todo: strip identical OpDecoration / OpDecorationGroups
}

#[derive(Clone, Debug)]
struct LinkSymbol {
    name: String,
    id: u32,
}

struct LinkInfo {
    imports: Vec<LinkSymbol>,
    exports: HashMap<String, Vec<LinkSymbol>>,
}

fn find_import_export_pairs(module: &rspirv::dr::Module) -> LinkInfo {
    let mut imports = vec![];
    let mut exports: HashMap<String, Vec<LinkSymbol>> = HashMap::new();

    for annotation in &module.annotations {
        if annotation.class.opcode == spirv::Op::Decorate
            && annotation.operands[1]
                == rspirv::dr::Operand::Decoration(spirv::Decoration::LinkageAttributes)
        {
            let id = match annotation.operands[0] {
                rspirv::dr::Operand::IdRef(i) => i,
                _ => panic!("Expected IdRef")
            };
            let name = &annotation.operands[2];
            let ty = &annotation.operands[3];

            let symbol = LinkSymbol {
                name: name.to_string(),
                id,
                validate the type of the function parameters and scan for OpVariable
            };

            if ty == &rspirv::dr::Operand::LinkageType(spirv::LinkageType::Import) {
                imports.push(symbol);
            } else {
                exports
                    .entry(symbol.name.clone())
                    .and_modify(|v| v.push(symbol.clone()))
                    .or_insert_with(|| vec![symbol.clone()]);
            }
        }
    }

    LinkInfo {
        imports,
        exports
    }
}

fn link(inputs: &mut [&mut rspirv::dr::Module]) -> () {
    // 1. shift all the ids
    let mut bound = inputs[0].header.as_ref().unwrap().bound - 1;

    for mut module in inputs.iter_mut().skip(1) {
        shift_ids(&mut module, bound);
        bound += module.header.as_ref().unwrap().bound - 1;
    }

    println!("{}\n\n", inputs[0].disassemble());
    println!("{}\n\n", inputs[1].disassemble());

    // 2. generate the header (todo)
    // 3. merge the binaries
    let mut loader = rspirv::dr::Loader::new();

    for module in inputs.iter() {
        module.all_inst_iter().for_each(|inst| {
            loader.consume_instruction(inst.clone());
        });
    }

    let mut output = loader.module();
    // 4. find import / export pairs
    find_import_export_pairs(&output);
    // 5. ensure import / export pairs have matching types and defintions
    // 6. remove duplicates (https://github.com/KhronosGroup/SPIRV-Tools/blob/e7866de4b1dc2a7e8672867caeb0bdca49f458d3/source/opt/remove_duplicates_pass.cpp)
    remove_duplicates(&mut output);


    // 7. remove names and decorations of import variables / functions https://github.com/KhronosGroup/SPIRV-Tools/blob/8a0ebd40f86d1f18ad42ea96c6ac53915076c3c7/source/opt/ir_context.cpp#L404
    // 8. rematch import variables and functions to export variables / functions https://github.com/KhronosGroup/SPIRV-Tools/blob/8a0ebd40f86d1f18ad42ea96c6ac53915076c3c7/source/opt/ir_context.cpp#L255
    // 9. remove linkage specific instructions
    // 10. compact the ids https://github.com/KhronosGroup/SPIRV-Tools/blob/e02f178a716b0c3c803ce31b9df4088596537872/source/opt/compact_ids_pass.cpp#L43
    // 11. output the module
    println!("{}\n\n", output.disassemble());
}

fn main() {
    let body1 = include_bytes!("../test/1/body_1.spv");
    let body2 = include_bytes!("../test/1/body_2.spv");

    let mut body1 = load(&body1[..]);
    let mut body2 = load(&body2[..]);

    link(&mut [&mut body1, &mut body2]);
}
