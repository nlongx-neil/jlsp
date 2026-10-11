use std::collections::{HashMap, HashSet};
use std::path::Path;
use cafebabe::attributes::{AttributeData, CodeData};
use cafebabe::bytecode::{Opcode, PrimitiveArrayType};
use cafebabe::constant_pool::{LiteralConstant, Loadable};
use crate::model::{ClassInfo, ClassKind, TypeSignature};

/// Clean JVM internal class/descriptor name (e.g. "java/lang/String" -> "String", "Ljava/util/List;" -> "List")
pub fn clean_class_name(s: &str) -> String {
    let raw = s.trim_start_matches('L').trim_end_matches(';');
    let simple = raw.split('/').last().unwrap_or(raw);
    simple.replace('$', ".")
}

/// Convert JVM descriptor char/string to Java type name
pub fn descriptor_to_simple_type(desc: &str) -> String {
    match desc {
        "V" => "void".to_string(),
        "Z" => "boolean".to_string(),
        "B" => "byte".to_string(),
        "C" => "char".to_string(),
        "S" => "short".to_string(),
        "I" => "int".to_string(),
        "J" => "long".to_string(),
        "F" => "float".to_string(),
        "D" => "double".to_string(),
        _ if desc.starts_with('[') => format!("{}[]", descriptor_to_simple_type(&desc[1..])),
        _ => clean_class_name(desc),
    }
}

fn indent_lines(code: &str, indent: &str) -> String {
    code.lines()
        .map(|l| {
            if l.trim().is_empty() {
                String::new()
            } else {
                format!("{}{}", indent, l)
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Decompile JVM bytecode inside a method's CodeData into readable Java statements
pub fn decompile_code_data(
    _method_name: &str,
    descriptor: &str,
    is_static: bool,
    _is_constructor: bool,
    class_simple_name: &str,
    code_data: &CodeData,
) -> Option<String> {
    let bytecode = code_data.bytecode.as_ref()?;
    let opcodes = &bytecode.opcodes;
    if opcodes.is_empty() {
        return Some(String::new());
    }

    // 1. Extract LocalVariableTable
    let mut local_vars: HashMap<u16, (String, String)> = HashMap::new();
    for attr in &code_data.attributes {
        if let AttributeData::LocalVariableTable(ref entries) = attr.data {
            for entry in entries {
                let var_name = entry.name.to_string();
                let var_type = descriptor_to_simple_type(&format!("{}", entry.descriptor));
                local_vars.insert(entry.index, (var_name, var_type));
            }
        }
    }

    if !is_static && !local_vars.contains_key(&0) {
        local_vars.insert(0, ("this".to_string(), class_simple_name.to_string()));
    }

    // 2. Map method parameter slots
    let (param_types, ret_type) = crate::classpath::descriptor::parse_method_descriptor(descriptor);
    let mut param_slots = HashSet::new();
    if !is_static {
        param_slots.insert(0);
    }
    let mut cur_slot = if is_static { 0 } else { 1 };
    for (idx, pty) in param_types.iter().enumerate() {
        param_slots.insert(cur_slot);
        local_vars
            .entry(cur_slot)
            .or_insert_with(|| (format!("arg{idx}"), pty.simple_name()));
        if matches!(pty, TypeSignature::Long | TypeSignature::Double) {
            cur_slot += 2;
        } else {
            cur_slot += 1;
        }
    }

    let get_var_name = |slot: u16, map: &HashMap<u16, (String, String)>| -> String {
        map.get(&slot)
            .map(|(n, _)| n.clone())
            .unwrap_or_else(|| format!("var{slot}"))
    };

    let get_var_type = |slot: u16, map: &HashMap<u16, (String, String)>| -> String {
        map.get(&slot)
            .map(|(_, t)| t.clone())
            .unwrap_or_else(|| "var".to_string())
    };

    // 3. Pre-scan forward jumps for if / if-else structure
    // pc -> (target_pc, is_if_else, else_end_pc)
    let mut forward_if_targets: HashMap<usize, (usize, bool, usize)> = HashMap::new();
    let mut block_closes: HashMap<usize, usize> = HashMap::new(); // target_pc -> count of braces to close
    let mut else_starts: HashMap<usize, usize> = HashMap::new(); // target_pc -> else_end_pc

    for (i, &(pc, ref op)) in opcodes.iter().enumerate() {
        let target_pc = match op {
            Opcode::IfIcmpeq(offset)
            | Opcode::IfIcmpne(offset)
            | Opcode::IfIcmplt(offset)
            | Opcode::IfIcmpge(offset)
            | Opcode::IfIcmpgt(offset)
            | Opcode::IfIcmple(offset)
            | Opcode::IfAcmpeq(offset)
            | Opcode::IfAcmpne(offset)
            | Opcode::Ifeq(offset)
            | Opcode::Ifne(offset)
            | Opcode::Iflt(offset)
            | Opcode::Ifge(offset)
            | Opcode::Ifgt(offset)
            | Opcode::Ifle(offset)
            | Opcode::Ifnull(offset)
            | Opcode::Ifnonnull(offset) => {
                let t = (pc as i32 + offset) as usize;
                if t > pc {
                    Some(t)
                } else {
                    None
                }
            }
            _ => None,
        };

        if let Some(target) = target_pc {
            // Check if instruction before target_pc is an unconditional goto
            let mut is_if_else = false;
            let mut else_end = target;
            if i + 1 < opcodes.len() {
                // Find instruction right before target
                if let Some(target_idx) = opcodes.iter().position(|&(p, _)| p == target) {
                    if target_idx > 0 {
                        if let Opcode::Goto(goto_offset) = opcodes[target_idx - 1].1 {
                            let goto_pc = opcodes[target_idx - 1].0;
                            let goto_dest = (goto_pc as i32 + goto_offset) as usize;
                            if goto_dest > target {
                                is_if_else = true;
                                else_end = goto_dest;
                            }
                        }
                    }
                }
            }

            forward_if_targets.insert(pc, (target, is_if_else, else_end));
            if is_if_else {
                else_starts.insert(target, else_end);
                *block_closes.entry(else_end).or_insert(0) += 1;
            } else {
                *block_closes.entry(target).or_insert(0) += 1;
            }
        }
    }

    // 4. Symbolic stack evaluation & statement emission
    let mut stack: Vec<String> = Vec::new();
    let mut statements: Vec<String> = Vec::new();
    let mut declared_vars: HashSet<u16> = param_slots.clone();
    let mut indent_level = 0;

    let format_stmt = |stmt: &str, level: usize| -> String {
        let prefix = "    ".repeat(level);
        format!("{}{}", prefix, stmt)
    };

    for &(pc, ref op) in opcodes {
        // Check if any block ends at this pc
        if let Some(else_end) = else_starts.remove(&pc) {
            if indent_level > 0 {
                indent_level -= 1;
            }
            statements.push(format_stmt("} else {", indent_level));
            indent_level += 1;
            // The else block will close at else_end
            let _ = else_end;
        } else if let Some(count) = block_closes.remove(&pc) {
            for _ in 0..count {
                if indent_level > 0 {
                    indent_level -= 1;
                }
                statements.push(format_stmt("}", indent_level));
            }
        }

        match op {
            Opcode::AconstNull => stack.push("null".into()),
            Opcode::IconstM1 => stack.push("-1".into()),
            Opcode::Iconst0 => {
                if ret_type == TypeSignature::Boolean && stack.is_empty() {
                    stack.push("false".into());
                } else {
                    stack.push("0".into());
                }
            }
            Opcode::Iconst1 => {
                if ret_type == TypeSignature::Boolean && stack.is_empty() {
                    stack.push("true".into());
                } else {
                    stack.push("1".into());
                }
            }
            Opcode::Iconst2 => stack.push("2".into()),
            Opcode::Iconst3 => stack.push("3".into()),
            Opcode::Iconst4 => stack.push("4".into()),
            Opcode::Iconst5 => stack.push("5".into()),
            Opcode::Lconst0 => stack.push("0L".into()),
            Opcode::Lconst1 => stack.push("1L".into()),
            Opcode::Fconst0 => stack.push("0.0f".into()),
            Opcode::Fconst1 => stack.push("1.0f".into()),
            Opcode::Fconst2 => stack.push("2.0f".into()),
            Opcode::Dconst0 => stack.push("0.0".into()),
            Opcode::Dconst1 => stack.push("1.0".into()),
            Opcode::Bipush(b) => stack.push(b.to_string()),
            Opcode::Sipush(s) => stack.push(s.to_string()),

            Opcode::Ldc(loadable) | Opcode::LdcW(loadable) | Opcode::Ldc2W(loadable) => {
                match loadable {
                    Loadable::LiteralConstant(LiteralConstant::String(s)) => {
                        stack.push(format!("{:?}", s));
                    }
                    Loadable::LiteralConstant(LiteralConstant::Integer(i)) => {
                        stack.push(i.to_string());
                    }
                    Loadable::LiteralConstant(LiteralConstant::Long(l)) => {
                        stack.push(format!("{l}L"));
                    }
                    Loadable::LiteralConstant(LiteralConstant::Float(f)) => {
                        stack.push(format!("{f}f"));
                    }
                    Loadable::LiteralConstant(LiteralConstant::Double(d)) => {
                        stack.push(format!("{d}"));
                    }
                    Loadable::ClassInfo(c) => {
                        stack.push(format!("{}.class", clean_class_name(c)));
                    }
                    _ => stack.push("null".into()),
                }
            }

            Opcode::Aload(slot)
            | Opcode::Iload(slot)
            | Opcode::Lload(slot)
            | Opcode::Fload(slot)
            | Opcode::Dload(slot) => {
                stack.push(get_var_name(*slot, &local_vars));
            }

            Opcode::Astore(slot)
            | Opcode::Istore(slot)
            | Opcode::Lstore(slot)
            | Opcode::Fstore(slot)
            | Opcode::Dstore(slot) => {
                let val = stack.pop().unwrap_or_else(|| "null".into());
                let var_name = get_var_name(*slot, &local_vars);
                if declared_vars.insert(*slot) && !param_slots.contains(slot) {
                    let var_type = get_var_type(*slot, &local_vars);
                    statements.push(format_stmt(&format!("{var_type} {var_name} = {val};"), indent_level));
                } else {
                    statements.push(format_stmt(&format!("{var_name} = {val};"), indent_level));
                }
            }

            Opcode::Getfield(member) => {
                let obj = stack.pop().unwrap_or_else(|| "this".into());
                let f_name = &member.name_and_type.name;
                if obj == "this" {
                    stack.push(format!("this.{f_name}"));
                } else {
                    stack.push(format!("{obj}.{f_name}"));
                }
            }

            Opcode::Getstatic(member) => {
                let owner = clean_class_name(&member.class_name);
                stack.push(format!("{owner}.{}", member.name_and_type.name));
            }

            Opcode::Putfield(member) => {
                let val = stack.pop().unwrap_or_else(|| "null".into());
                let obj = stack.pop().unwrap_or_else(|| "this".into());
                statements.push(format_stmt(&format!("{obj}.{} = {val};", member.name_and_type.name), indent_level));
            }

            Opcode::Putstatic(member) => {
                let val = stack.pop().unwrap_or_else(|| "null".into());
                let owner = clean_class_name(&member.class_name);
                statements.push(format_stmt(&format!("{owner}.{} = {val};", member.name_and_type.name), indent_level));
            }

            Opcode::New(cls) => {
                stack.push(format!("__NEW__{}", cls));
            }

            Opcode::Dup => {
                if let Some(top) = stack.last() {
                    if !top.starts_with("__NEW__") {
                        stack.push(top.clone());
                    }
                }
            }

            Opcode::Invokespecial(member) => {
                let (arg_types, m_ret) = crate::classpath::descriptor::parse_method_descriptor(&member.name_and_type.descriptor);
                let mut args = Vec::new();
                for _ in 0..arg_types.len() {
                    if let Some(arg) = stack.pop() {
                        args.push(arg);
                    }
                }
                args.reverse();

                let receiver = stack.pop().unwrap_or_else(|| "this".into());
                if receiver.starts_with("__NEW__") {
                    let cls = clean_class_name(&member.class_name);
                    stack.push(format!("new {cls}({})", args.join(", ")));
                } else if member.name_and_type.name == "<init>" {
                    let owner = clean_class_name(&member.class_name);
                    if receiver == "this" && owner == class_simple_name {
                        statements.push(format_stmt(&format!("this({});", args.join(", ")), indent_level));
                    } else if owner != "Object" || !args.is_empty() {
                        statements.push(format_stmt(&format!("super({});", args.join(", ")), indent_level));
                    }
                } else {
                    let call = format!("{receiver}.{}({})", member.name_and_type.name, args.join(", "));
                    if m_ret == TypeSignature::Void {
                        statements.push(format_stmt(&format!("{call};"), indent_level));
                    } else {
                        stack.push(call);
                    }
                }
            }

            Opcode::Invokevirtual(member) | Opcode::Invokeinterface(member, _) => {
                let (arg_types, m_ret) = crate::classpath::descriptor::parse_method_descriptor(&member.name_and_type.descriptor);
                let mut args = Vec::new();
                for _ in 0..arg_types.len() {
                    if let Some(arg) = stack.pop() {
                        args.push(arg);
                    }
                }
                args.reverse();

                let receiver = stack.pop().unwrap_or_else(|| "this".into());
                let m_name = &member.name_and_type.name;
                let owner = clean_class_name(&member.class_name);

                // Handle StringBuilder.append(...) in Java string concatenation
                if owner == "StringBuilder" && m_name == "append" {
                    let arg_str = args.into_iter().next().unwrap_or_default();
                    if receiver == "new StringBuilder()" || receiver.starts_with("__NEW__") {
                        stack.push(arg_str);
                    } else {
                        stack.push(format!("{receiver} + {arg_str}"));
                    }
                } else if owner == "StringBuilder" && m_name == "toString" {
                    stack.push(receiver);
                } else {
                    let call = format!("{receiver}.{m_name}({})", args.join(", "));
                    if m_ret == TypeSignature::Void {
                        statements.push(format_stmt(&format!("{call};"), indent_level));
                    } else {
                        stack.push(call);
                    }
                }
            }

            Opcode::Invokestatic(member) => {
                let (arg_types, m_ret) = crate::classpath::descriptor::parse_method_descriptor(&member.name_and_type.descriptor);
                let mut args = Vec::new();
                for _ in 0..arg_types.len() {
                    if let Some(arg) = stack.pop() {
                        args.push(arg);
                    }
                }
                args.reverse();

                let owner = clean_class_name(&member.class_name);
                let call = format!("{owner}.{}({})", member.name_and_type.name, args.join(", "));
                if m_ret == TypeSignature::Void {
                    statements.push(format_stmt(&format!("{call};"), indent_level));
                } else {
                    stack.push(call);
                }
            }

            Opcode::Invokedynamic(dyn_call) => {
                let (arg_types, m_ret) = crate::classpath::descriptor::parse_method_descriptor(&dyn_call.name_and_type.descriptor);
                let mut args = Vec::new();
                for _ in 0..arg_types.len() {
                    if let Some(arg) = stack.pop() {
                        args.push(arg);
                    }
                }
                args.reverse();

                if dyn_call.name_and_type.name.contains("makeConcat") {
                    stack.push(args.join(" + "));
                } else {
                    let call = format!("{}({})", dyn_call.name_and_type.name, args.join(", "));
                    if m_ret == TypeSignature::Void {
                        statements.push(format_stmt(&format!("{call};"), indent_level));
                    } else {
                        stack.push(call);
                    }
                }
            }

            Opcode::Iadd | Opcode::Ladd | Opcode::Fadd | Opcode::Dadd => {
                let b = stack.pop().unwrap_or_default();
                let a = stack.pop().unwrap_or_default();
                stack.push(format!("{a} + {b}"));
            }
            Opcode::Isub | Opcode::Lsub | Opcode::Fsub | Opcode::Dsub => {
                let b = stack.pop().unwrap_or_default();
                let a = stack.pop().unwrap_or_default();
                stack.push(format!("{a} - {b}"));
            }
            Opcode::Imul | Opcode::Lmul | Opcode::Fmul | Opcode::Dmul => {
                let b = stack.pop().unwrap_or_default();
                let a = stack.pop().unwrap_or_default();
                stack.push(format!("{a} * {b}"));
            }
            Opcode::Idiv | Opcode::Ldiv | Opcode::Fdiv | Opcode::Ddiv => {
                let b = stack.pop().unwrap_or_default();
                let a = stack.pop().unwrap_or_default();
                stack.push(format!("{a} / {b}"));
            }
            Opcode::Irem | Opcode::Lrem | Opcode::Frem | Opcode::Drem => {
                let b = stack.pop().unwrap_or_default();
                let a = stack.pop().unwrap_or_default();
                stack.push(format!("{a} % {b}"));
            }
            Opcode::Ineg | Opcode::Lneg | Opcode::Fneg | Opcode::Dneg => {
                let a = stack.pop().unwrap_or_default();
                stack.push(format!("-{a}"));
            }
            Opcode::Iand | Opcode::Land => {
                let b = stack.pop().unwrap_or_default();
                let a = stack.pop().unwrap_or_default();
                stack.push(format!("{a} & {b}"));
            }
            Opcode::Ior | Opcode::Lor => {
                let b = stack.pop().unwrap_or_default();
                let a = stack.pop().unwrap_or_default();
                stack.push(format!("{a} | {b}"));
            }
            Opcode::Ixor | Opcode::Lxor => {
                let b = stack.pop().unwrap_or_default();
                let a = stack.pop().unwrap_or_default();
                stack.push(format!("{a} ^ {b}"));
            }
            Opcode::Ishl | Opcode::Lshl => {
                let b = stack.pop().unwrap_or_default();
                let a = stack.pop().unwrap_or_default();
                stack.push(format!("{a} << {b}"));
            }
            Opcode::Ishr | Opcode::Lshr => {
                let b = stack.pop().unwrap_or_default();
                let a = stack.pop().unwrap_or_default();
                stack.push(format!("{a} >> {b}"));
            }
            Opcode::Iushr | Opcode::Lushr => {
                let b = stack.pop().unwrap_or_default();
                let a = stack.pop().unwrap_or_default();
                stack.push(format!("{a} >>> {b}"));
            }

            Opcode::Iinc(slot, incr) => {
                let name = get_var_name(*slot, &local_vars);
                if *incr == 1 {
                    statements.push(format_stmt(&format!("{name}++;"), indent_level));
                } else if *incr == -1 {
                    statements.push(format_stmt(&format!("{name}--;"), indent_level));
                } else if *incr > 0 {
                    statements.push(format_stmt(&format!("{name} += {incr};"), indent_level));
                } else {
                    let abs_val = (*incr as i32).unsigned_abs();
                    statements.push(format_stmt(&format!("{name} -= {abs_val};"), indent_level));
                }
            }

            Opcode::Newarray(prim) => {
                let len = stack.pop().unwrap_or_default();
                let p_name = match prim {
                    PrimitiveArrayType::Boolean => "boolean",
                    PrimitiveArrayType::Char => "char",
                    PrimitiveArrayType::Float => "float",
                    PrimitiveArrayType::Double => "double",
                    PrimitiveArrayType::Byte => "byte",
                    PrimitiveArrayType::Short => "short",
                    PrimitiveArrayType::Int => "int",
                    PrimitiveArrayType::Long => "long",
                };
                stack.push(format!("new {p_name}[{len}]"));
            }

            Opcode::Anewarray(elem) => {
                let len = stack.pop().unwrap_or_default();
                let el = clean_class_name(&format!("{:?}", elem));
                stack.push(format!("new {el}[{len}]"));
            }

            Opcode::Arraylength => {
                let arr = stack.pop().unwrap_or_default();
                stack.push(format!("{arr}.length"));
            }

            Opcode::Aaload
            | Opcode::Iaload
            | Opcode::Laload
            | Opcode::Faload
            | Opcode::Daload
            | Opcode::Baload
            | Opcode::Caload
            | Opcode::Saload => {
                let idx = stack.pop().unwrap_or_default();
                let arr = stack.pop().unwrap_or_default();
                stack.push(format!("{arr}[{idx}]"));
            }

            Opcode::Aastore
            | Opcode::Iastore
            | Opcode::Lastore
            | Opcode::Fastore
            | Opcode::Dastore
            | Opcode::Bastore
            | Opcode::Castore
            | Opcode::Sastore => {
                let val = stack.pop().unwrap_or_default();
                let idx = stack.pop().unwrap_or_default();
                let arr = stack.pop().unwrap_or_default();
                statements.push(format_stmt(&format!("{arr}[{idx}] = {val};"), indent_level));
            }

            Opcode::Checkcast(target) => {
                let val = stack.pop().unwrap_or_default();
                let target_name = clean_class_name(&format!("{:?}", target));
                stack.push(format!("({target_name}) {val}"));
            }

            Opcode::Instanceof(target) => {
                let val = stack.pop().unwrap_or_default();
                let target_name = clean_class_name(&format!("{:?}", target));
                stack.push(format!("{val} instanceof {target_name}"));
            }

            Opcode::Return => {
                // Only emit return if not the trailing instruction at root level
                if indent_level > 0 {
                    statements.push(format_stmt("return;", indent_level));
                }
            }

            Opcode::Ireturn => {
                let val = stack.pop().unwrap_or_else(|| "0".into());
                let ret_expr = if ret_type == TypeSignature::Boolean {
                    if val == "0" {
                        "false".to_string()
                    } else if val == "1" {
                        "true".to_string()
                    } else {
                        val
                    }
                } else {
                    val
                };
                statements.push(format_stmt(&format!("return {ret_expr};"), indent_level));
            }

            Opcode::Areturn | Opcode::Lreturn | Opcode::Freturn | Opcode::Dreturn => {
                let val = stack.pop().unwrap_or_else(|| "null".into());
                statements.push(format_stmt(&format!("return {val};"), indent_level));
            }

            Opcode::Athrow => {
                let ex = stack.pop().unwrap_or_else(|| "new RuntimeException()".into());
                statements.push(format_stmt(&format!("throw {ex};"), indent_level));
            }

            Opcode::Pop | Opcode::Pop2 => {
                if let Some(expr) = stack.pop() {
                    if !expr.starts_with("__NEW__") {
                        statements.push(format_stmt(&format!("{expr};"), indent_level));
                    }
                }
            }

            // Forward conditionals
            Opcode::IfIcmpeq(_)
            | Opcode::IfIcmpne(_)
            | Opcode::IfIcmplt(_)
            | Opcode::IfIcmpge(_)
            | Opcode::IfIcmpgt(_)
            | Opcode::IfIcmple(_)
            | Opcode::IfAcmpeq(_)
            | Opcode::IfAcmpne(_)
            | Opcode::Ifeq(_)
            | Opcode::Ifne(_)
            | Opcode::Iflt(_)
            | Opcode::Ifge(_)
            | Opcode::Ifgt(_)
            | Opcode::Ifle(_)
            | Opcode::Ifnull(_)
            | Opcode::Ifnonnull(_) => {
                let cond = match op {
                    Opcode::IfIcmpeq(_) | Opcode::IfAcmpeq(_) => {
                        let b = stack.pop().unwrap_or_default();
                        let a = stack.pop().unwrap_or_default();
                        format!("{a} != {b}")
                    }
                    Opcode::IfIcmpne(_) | Opcode::IfAcmpne(_) => {
                        let b = stack.pop().unwrap_or_default();
                        let a = stack.pop().unwrap_or_default();
                        format!("{a} == {b}")
                    }
                    Opcode::IfIcmplt(_) => {
                        let b = stack.pop().unwrap_or_default();
                        let a = stack.pop().unwrap_or_default();
                        format!("{a} >= {b}")
                    }
                    Opcode::IfIcmpge(_) => {
                        let b = stack.pop().unwrap_or_default();
                        let a = stack.pop().unwrap_or_default();
                        format!("{a} < {b}")
                    }
                    Opcode::IfIcmpgt(_) => {
                        let b = stack.pop().unwrap_or_default();
                        let a = stack.pop().unwrap_or_default();
                        format!("{a} <= {b}")
                    }
                    Opcode::IfIcmple(_) => {
                        let b = stack.pop().unwrap_or_default();
                        let a = stack.pop().unwrap_or_default();
                        format!("{a} > {b}")
                    }
                    Opcode::Ifeq(_) => {
                        let a = stack.pop().unwrap_or_default();
                        if a == "true" { "false".into() } else if a == "false" { "true".into() } else { format!("{a} != 0") }
                    }
                    Opcode::Ifne(_) => {
                        let a = stack.pop().unwrap_or_default();
                        if a == "true" { "true".into() } else if a == "false" { "false".into() } else { format!("{a} == 0") }
                    }
                    Opcode::Iflt(_) => {
                        let a = stack.pop().unwrap_or_default();
                        format!("{a} >= 0")
                    }
                    Opcode::Ifge(_) => {
                        let a = stack.pop().unwrap_or_default();
                        format!("{a} < 0")
                    }
                    Opcode::Ifgt(_) => {
                        let a = stack.pop().unwrap_or_default();
                        format!("{a} <= 0")
                    }
                    Opcode::Ifle(_) => {
                        let a = stack.pop().unwrap_or_default();
                        format!("{a} > 0")
                    }
                    Opcode::Ifnull(_) => {
                        let a = stack.pop().unwrap_or_default();
                        format!("{a} != null")
                    }
                    Opcode::Ifnonnull(_) => {
                        let a = stack.pop().unwrap_or_default();
                        format!("{a} == null")
                    }
                    _ => unreachable!(),
                };

                statements.push(format_stmt(&format!("if ({cond}) {{"), indent_level));
                indent_level += 1;
            }

            Opcode::Goto(offset) => {
                let target = (pc as i32 + offset) as usize;
                if target < pc {
                    // Backward jump = loop!
                    statements.push(format_stmt("continue;", indent_level));
                }
            }

            _ => {}
        }
    }

    // Close any remaining open blocks
    while indent_level > 0 {
        indent_level -= 1;
        statements.push(format_stmt("}", indent_level));
    }

    Some(statements.join("\n"))
}

/// Generate human-readable Java source from binary ClassInfo
pub fn decompile_class_info(cls: &ClassInfo) -> String {
    let mut out = String::new();

    // 1. Package statement
    if !cls.package_name.is_empty() {
        out.push_str(&format!("package {};\n\n", cls.package_name));
    }

    out.push_str("// [READ-ONLY] Decompiled by jlsp (pure Rust, JVM-free). DO NOT EDIT.\n");

    // Standard annotations for core Spring annotations if decompiling builtin
    if cls.fqn == "org.springframework.web.bind.annotation.RestController" {
        out.push_str("import java.lang.annotation.Documented;\n");
        out.push_str("import java.lang.annotation.ElementType;\n");
        out.push_str("import java.lang.annotation.Retention;\n");
        out.push_str("import java.lang.annotation.RetentionPolicy;\n");
        out.push_str("import java.lang.annotation.Target;\n");
        out.push_str("import org.springframework.stereotype.Controller;\n");
        out.push_str("import org.springframework.web.bind.annotation.ResponseBody;\n\n");
        out.push_str("@Target(ElementType.TYPE)\n");
        out.push_str("@Retention(RetentionPolicy.RUNTIME)\n");
        out.push_str("@Documented\n");
        out.push_str("@Controller\n");
        out.push_str("@ResponseBody\n");
    } else if cls.fqn == "org.springframework.stereotype.Controller" {
        out.push_str("import java.lang.annotation.Documented;\n");
        out.push_str("import java.lang.annotation.ElementType;\n");
        out.push_str("import java.lang.annotation.Retention;\n");
        out.push_str("import java.lang.annotation.RetentionPolicy;\n");
        out.push_str("import java.lang.annotation.Target;\n");
        out.push_str("import org.springframework.stereotype.Component;\n\n");
        out.push_str("@Target(ElementType.TYPE)\n");
        out.push_str("@Retention(RetentionPolicy.RUNTIME)\n");
        out.push_str("@Documented\n");
        out.push_str("@Component\n");
    } else if cls.fqn == "org.springframework.web.bind.annotation.ResponseBody" {
        out.push_str("import java.lang.annotation.Documented;\n");
        out.push_str("import java.lang.annotation.ElementType;\n");
        out.push_str("import java.lang.annotation.Retention;\n");
        out.push_str("import java.lang.annotation.RetentionPolicy;\n");
        out.push_str("import java.lang.annotation.Target;\n\n");
        out.push_str("@Target({ElementType.TYPE, ElementType.METHOD})\n");
        out.push_str("@Retention(RetentionPolicy.RUNTIME)\n");
        out.push_str("@Documented\n");
    } else if cls.fqn == "org.apache.ibatis.annotations.Param" {
        out.push_str("import java.lang.annotation.Documented;\n");
        out.push_str("import java.lang.annotation.ElementType;\n");
        out.push_str("import java.lang.annotation.Retention;\n");
        out.push_str("import java.lang.annotation.RetentionPolicy;\n");
        out.push_str("import java.lang.annotation.Target;\n\n");
        out.push_str("@Documented\n");
        out.push_str("@Retention(RetentionPolicy.RUNTIME)\n");
        out.push_str("@Target(ElementType.PARAMETER)\n");
    } else if cls.fqn == "org.apache.ibatis.annotations.Update" {
        out.push_str("import java.lang.annotation.Documented;\n");
        out.push_str("import java.lang.annotation.ElementType;\n");
        out.push_str("import java.lang.annotation.Retention;\n");
        out.push_str("import java.lang.annotation.RetentionPolicy;\n");
        out.push_str("import java.lang.annotation.Target;\n\n");
        out.push_str("@Documented\n");
        out.push_str("@Retention(RetentionPolicy.RUNTIME)\n");
        out.push_str("@Target(ElementType.METHOD)\n");
    } else if cls.fqn == "org.apache.ibatis.annotations.Select" {
        out.push_str("import java.lang.annotation.Documented;\n");
        out.push_str("import java.lang.annotation.ElementType;\n");
        out.push_str("import java.lang.annotation.Retention;\n");
        out.push_str("import java.lang.annotation.RetentionPolicy;\n");
        out.push_str("import java.lang.annotation.Target;\n\n");
        out.push_str("@Documented\n");
        out.push_str("@Retention(RetentionPolicy.RUNTIME)\n");
        out.push_str("@Target(ElementType.METHOD)\n");
    } else if cls.fqn == "org.apache.ibatis.annotations.Insert" {
        out.push_str("import java.lang.annotation.Documented;\n");
        out.push_str("import java.lang.annotation.ElementType;\n");
        out.push_str("import java.lang.annotation.Retention;\n");
        out.push_str("import java.lang.annotation.RetentionPolicy;\n");
        out.push_str("import java.lang.annotation.Target;\n\n");
        out.push_str("@Documented\n");
        out.push_str("@Retention(RetentionPolicy.RUNTIME)\n");
        out.push_str("@Target(ElementType.METHOD)\n");
    } else if cls.fqn == "org.apache.ibatis.annotations.Delete" {
        out.push_str("import java.lang.annotation.Documented;\n");
        out.push_str("import java.lang.annotation.ElementType;\n");
        out.push_str("import java.lang.annotation.Retention;\n");
        out.push_str("import java.lang.annotation.RetentionPolicy;\n");
        out.push_str("import java.lang.annotation.Target;\n\n");
        out.push_str("@Documented\n");
        out.push_str("@Retention(RetentionPolicy.RUNTIME)\n");
        out.push_str("@Target(ElementType.METHOD)\n");
    } else if cls.fqn == "org.apache.ibatis.annotations.Mapper" {
        out.push_str("import java.lang.annotation.Documented;\n");
        out.push_str("import java.lang.annotation.ElementType;\n");
        out.push_str("import java.lang.annotation.Inherited;\n");
        out.push_str("import java.lang.annotation.Retention;\n");
        out.push_str("import java.lang.annotation.RetentionPolicy;\n");
        out.push_str("import java.lang.annotation.Target;\n\n");
        out.push_str("@Documented\n");
        out.push_str("@Inherited\n");
        out.push_str("@Retention(RetentionPolicy.RUNTIME)\n");
        out.push_str("@Target(ElementType.TYPE)\n");
    }

    // 2. Class signature
    let kind_str = match cls.kind {
        ClassKind::Interface => "interface",
        ClassKind::Enum => "enum",
        ClassKind::Record => "record",
        ClassKind::Annotation => "@interface",
        ClassKind::Class => "class",
    };

    let visibility = if cls.is_public { "public " } else { "" };
    out.push_str(&format!("{}{} {}", visibility, kind_str, cls.simple_name));

    if cls.kind != ClassKind::Annotation {
        if let Some(super_cls) = &cls.super_class {
            if super_cls != "java.lang.Object" {
                let simple_super = super_cls.split('.').last().unwrap_or(super_cls);
                out.push_str(&format!(" extends {}", simple_super));
            }
        }

        if !cls.interfaces.is_empty() {
            let ifaces: Vec<String> = cls
                .interfaces
                .iter()
                .filter(|i| *i != "java.lang.annotation.Annotation")
                .map(|i| i.split('.').last().unwrap_or(i).to_string())
                .collect();
            if !ifaces.is_empty() {
                out.push_str(&format!(" implements {}", ifaces.join(", ")));
            }
        }
    }

    out.push_str(" {\n");

    // 3. Fields
    for field in &cls.fields {
        let vis = if field.is_public { "    public " } else { "    " };
        let stat = if field.is_static { "static " } else { "" };
        out.push_str(&format!(
            "{}{}{}{};\n",
            vis,
            stat,
            format!("{} ", field.field_type.simple_name()),
            field.name
        ));
    }

    if !cls.fields.is_empty() && !cls.methods.is_empty() {
        out.push('\n');
    }

    // 4. Methods
    for method in &cls.methods {
        let vis = if method.is_public { "    public " } else { "    " };
        let stat = if method.is_static { "static " } else { "" };
        let param_strs: Vec<String> = method
            .parameters
            .iter()
            .map(|(pname, pty)| format!("{} {}", pty.simple_name(), pname))
            .collect();

        if cls.kind == ClassKind::Interface || cls.kind == ClassKind::Annotation {
            let default_clause = if let Some(body) = &method.body {
                let trimmed = body.trim();
                if trimmed.starts_with("default") {
                    format!(" {}", trimmed)
                } else {
                    String::new()
                }
            } else {
                String::new()
            };
            out.push_str(&format!(
                "{}{}{} {}({}){};\n",
                vis,
                stat,
                method.return_type.simple_name(),
                method.name,
                param_strs.join(", "),
                default_clause
            ));
        } else if let Some(body) = &method.body {
            let body_trimmed = body.trim();
            if body_trimmed.is_empty() {
                if method.is_constructor {
                    out.push_str(&format!(
                        "{}{}({}) {{\n    }}\n",
                        vis,
                        cls.simple_name,
                        param_strs.join(", ")
                    ));
                } else {
                    out.push_str(&format!(
                        "{}{}{} {}({}) {{\n    }}\n",
                        vis,
                        stat,
                        method.return_type.simple_name(),
                        method.name,
                        param_strs.join(", ")
                    ));
                }
            } else {
                let indented = indent_lines(body_trimmed, "        ");
                if method.is_constructor {
                    out.push_str(&format!(
                        "{}{}({}) {{\n{}\n    }}\n",
                        vis,
                        cls.simple_name,
                        param_strs.join(", "),
                        indented
                    ));
                } else {
                    out.push_str(&format!(
                        "{}{}{} {}({}) {{\n{}\n    }}\n",
                        vis,
                        stat,
                        method.return_type.simple_name(),
                        method.name,
                        param_strs.join(", "),
                        indented
                    ));
                }
            }
        } else {
            if method.is_constructor {
                out.push_str(&format!(
                    "{}{}({}) {{ /* compiled code */ }}\n",
                    vis,
                    cls.simple_name,
                    param_strs.join(", ")
                ));
            } else {
                out.push_str(&format!(
                    "{}{}{} {}({}) {{ /* compiled code */ }}\n",
                    vis,
                    stat,
                    method.return_type.simple_name(),
                    method.name,
                    param_strs.join(", ")
                ));
            }
        }
    }

    out.push_str("}\n");
    out
}

/// Decompile a class directly from raw .class file bytes
pub fn decompile_class_bytes(bytes: &[u8]) -> Result<String, String> {
    let cls_info = crate::classpath::class_reader::parse_class_bytes(bytes)?;
    Ok(decompile_class_info(&cls_info))
}

/// Decompile a class directly by extracting its .class entry from a JAR archive
pub fn decompile_class_from_jar(jar_path: &Path, fqn: &str) -> Option<String> {
    let file = std::fs::File::open(jar_path).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;
    let entry_name = format!("{}.class", fqn.replace('.', "/"));
    let mut zip_entry = archive.by_name(&entry_name).ok()?;
    let mut bytes = Vec::with_capacity(zip_entry.size() as usize);
    std::io::Read::read_to_end(&mut zip_entry, &mut bytes).ok()?;
    decompile_class_bytes(&bytes).ok()
}

/// Try to locate and extract real source code for a class from local *-sources.jar
pub fn extract_source_from_sources_jar(jar_path: &Path, fqn: &str) -> Option<String> {
    let file_name = jar_path.file_name()?.to_str()?;
    if !file_name.ends_with(".jar") {
        return None;
    }

    // Try finding sibling or matching *-sources.jar in the same directory or parent tree
    let sources_jar_name = file_name.replace(".jar", "-sources.jar");
    let sources_jar_path = jar_path.with_file_name(sources_jar_name);

    let target_sources_jar = if sources_jar_path.exists() {
        Some(sources_jar_path)
    } else if let Some(parent) = jar_path.parent() {
        // In gradle cache, sources.jar might be in a sibling hash directory
        let mut found = None;
        if let Some(grandparent) = parent.parent() {
            for entry in walkdir::WalkDir::new(grandparent).max_depth(3).into_iter().filter_map(|e| e.ok()) {
                let p = entry.path();
                if let Some(n) = p.file_name().and_then(|s| s.to_str()) {
                    if n.ends_with("-sources.jar") {
                        found = Some(p.to_path_buf());
                        break;
                    }
                }
            }
        }
        found
    } else {
        None
    };

    let target_sources_jar = target_sources_jar.or_else(|| {
        // Fallback: search user's .m2 or .gradle caches for matching artifact sources jar
        let home = std::env::var("HOME").ok().map(std::path::PathBuf::from)?;
        let artifact_part = file_name.strip_suffix(".jar")?;
        let expected_name = format!("{}-sources.jar", artifact_part);

        let search_dirs = [
            home.join(".m2/repository"),
            home.join(".gradle/caches/modules-2/files-2.1"),
        ];
        for base in search_dirs {
            if base.exists() {
                for entry in walkdir::WalkDir::new(base).max_depth(6).into_iter().filter_map(|e| e.ok()) {
                    let p = entry.path();
                    if p.file_name().and_then(|s| s.to_str()) == Some(&expected_name) {
                        return Some(p.to_path_buf());
                    }
                }
            }
        }
        None
    });

    let sources_jar = target_sources_jar?;
    let file = std::fs::File::open(&sources_jar).ok()?;
    let mut archive = zip::ZipArchive::new(file).ok()?;

    let entry_name = format!("{}.java", fqn.replace('.', "/"));
    let mut zip_entry = archive.by_name(&entry_name).ok()?;

    let mut content = String::new();
    std::io::Read::read_to_string(&mut zip_entry, &mut content).ok()?;
    Some(content)
}

/// Try to locate and extract real source code for a JDK class from JDK's src.zip
pub fn extract_source_from_jdk(jdk_home_opt: Option<&Path>, fqn: &str) -> Option<String> {
    let mut candidate_src_zips = Vec::new();

    // 1. From explicitly provided jdk_home
    if let Some(jdk_home) = jdk_home_opt {
        candidate_src_zips.push(jdk_home.join("lib").join("src.zip"));
        candidate_src_zips.push(jdk_home.join("src.zip"));
        candidate_src_zips.push(jdk_home.join("jre").join("lib").join("src.zip"));
    }

    // 2. From find_java_home()
    if let Some(jh) = crate::classpath::find_java_home() {
        candidate_src_zips.push(jh.join("lib").join("src.zip"));
        candidate_src_zips.push(jh.join("src.zip"));
        candidate_src_zips.push(jh.join("jre").join("lib").join("src.zip"));
    }

    // 3. Fallback checks from env JAVA_HOME / JDK_HOME
    for env_k in ["JAVA_HOME", "JDK_HOME"] {
        if let Ok(v) = std::env::var(env_k) {
            let p = std::path::PathBuf::from(v);
            candidate_src_zips.push(p.join("lib").join("src.zip"));
            candidate_src_zips.push(p.join("src.zip"));
        }
    }

    // 4. SDKMAN current
    if let Some(home) = crate::classpath::dirs_home() {
        candidate_src_zips.push(home.join(".sdkman/candidates/java/current/lib/src.zip"));
        candidate_src_zips.push(home.join(".sdkman/candidates/java/current/src.zip"));
    }

    #[cfg(target_os = "macos")]
    {
        let jvm_dir = Path::new("/Library/Java/JavaVirtualMachines");
        if jvm_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(jvm_dir) {
                for entry in entries.flatten() {
                    candidate_src_zips.push(entry.path().join("Contents/Home/lib/src.zip"));
                    candidate_src_zips.push(entry.path().join("Contents/Home/src.zip"));
                }
            }
        }
    }

    let rel_path = format!("{}.java", fqn.replace('.', "/"));
    let java_base_path = format!("java.base/{}", rel_path);

    for src_zip in candidate_src_zips {
        if !src_zip.exists() {
            continue;
        }
        let file = match std::fs::File::open(&src_zip) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let mut archive = match zip::ZipArchive::new(file) {
            Ok(a) => a,
            Err(_) => continue,
        };

        // Priority A: JDK 9+ java.base (covers java.lang, java.io, java.util, java.net, etc.)
        if let Ok(mut entry) = archive.by_name(&java_base_path) {
            let mut content = String::new();
            if std::io::Read::read_to_string(&mut entry, &mut content).is_ok() {
                return Some(content);
            }
        }

        // Priority B: JDK 6~8 flat structure (e.g. java/io/File.java)
        if let Ok(mut entry) = archive.by_name(&rel_path) {
            let mut content = String::new();
            if std::io::Read::read_to_string(&mut entry, &mut content).is_ok() {
                return Some(content);
            }
        }

        // Priority C: Other common JDK 9+ modules
        let common_modules = [
            "java.desktop", "java.sql", "java.xml", "java.logging",
            "java.management", "java.naming", "java.net.http",
            "java.compiler", "jdk.compiler", "jdk.javadoc", "jdk.httpserver",
        ];
        let mut found = false;
        for m in common_modules {
            let m_path = format!("{}/{}", m, rel_path);
            if let Ok(mut entry) = archive.by_name(&m_path) {
                let mut content = String::new();
                if std::io::Read::read_to_string(&mut entry, &mut content).is_ok() {
                    return Some(content);
                }
                found = true;
                break;
            }
        }
        if found {
            continue;
        }

        // Priority D: Fallback scanning entry names
        let matched = archive.file_names()
            .find(|name| name.ends_with(&format!("/{}", rel_path)) || *name == rel_path)
            .map(|s| s.to_string());
        if let Some(entry_name) = matched {
            if let Ok(mut entry) = archive.by_name(&entry_name) {
                let mut content = String::new();
                if std::io::Read::read_to_string(&mut entry, &mut content).is_ok() {
                    return Some(content);
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{FieldInfo, MethodInfo, TypeSignature};

    #[test]
    fn test_decompile_simple_class() {
        let cls = ClassInfo {
            fqn: "java.lang.String".into(),
            simple_name: "String".into(),
            package_name: "java.lang".into(),
            kind: ClassKind::Class,
            super_class: Some("java.lang.Object".into()),
            interfaces: vec!["java.io.Serializable".into()],
            methods: vec![MethodInfo {
                name: "length".into(),
                return_type: TypeSignature::Int,
                parameters: vec![],
                is_static: false,
                is_public: true,
                is_constructor: false,
                javadoc: None,
                body: None,
            }],
            fields: vec![FieldInfo {
                name: "value".into(),
                field_type: TypeSignature::Array(Box::new(TypeSignature::Byte)),
                is_static: false,
                is_public: false,
                is_final: false,
            }],
            source_path: None,
            is_public: true,
        };

        let decompiled = decompile_class_info(&cls);
        assert!(decompiled.contains("package java.lang;"));
        assert!(decompiled.contains("public class String implements Serializable {"));
        assert!(decompiled.contains("public int length() { /* compiled code */ }"));
    }

    #[test]
    fn test_decompile_with_real_method_body() {
        let cls = ClassInfo {
            fqn: "com.example.Calculator".into(),
            simple_name: "Calculator".into(),
            package_name: "com.example".into(),
            kind: ClassKind::Class,
            super_class: Some("java.lang.Object".into()),
            interfaces: vec![],
            methods: vec![
                MethodInfo {
                    name: "<init>".into(),
                    return_type: TypeSignature::Void,
                    parameters: vec![("base".into(), TypeSignature::Int)],
                    is_static: false,
                    is_public: true,
                    is_constructor: true,
                    javadoc: None,
                    body: Some("this.base = base;".into()),
                },
                MethodInfo {
                    name: "add".into(),
                    return_type: TypeSignature::Int,
                    parameters: vec![("a".into(), TypeSignature::Int), ("b".into(), TypeSignature::Int)],
                    is_static: false,
                    is_public: true,
                    is_constructor: false,
                    javadoc: None,
                    body: Some("return this.base + a + b;".into()),
                },
                MethodInfo {
                    name: "max".into(),
                    return_type: TypeSignature::Int,
                    parameters: vec![("x".into(), TypeSignature::Int), ("y".into(), TypeSignature::Int)],
                    is_static: false,
                    is_public: true,
                    is_constructor: false,
                    javadoc: None,
                    body: Some("if (x > y) {\n    return x;\n}\nreturn y;".into()),
                },
            ],
            fields: vec![FieldInfo {
                name: "base".into(),
                field_type: TypeSignature::Int,
                is_static: false,
                is_public: false,
                is_final: false,
            }],
            source_path: None,
            is_public: true,
        };

        let decompiled = decompile_class_info(&cls);
        assert!(decompiled.contains("public class Calculator {"));
        assert!(decompiled.contains("this.base = base;"));
        assert!(decompiled.contains("return this.base + a + b;"));
        assert!(decompiled.contains("if (x > y) {"));
    }

    #[test]
    fn test_decompile_real_compiled_bytecode() {
        let temp_dir = std::env::temp_dir().join("jlsp_real_bytecode_test");
        let _ = std::fs::create_dir_all(&temp_dir);
        let src_file = temp_dir.join("Service.java");
        let class_file = temp_dir.join("Service.class");

        let java_code = r#"public class Service {
    private int counter;
    public Service(int init) {
        this.counter = init;
    }
    public int incrementAndGet(int step) {
        return this.counter + step;
    }
    public int check(int a, int b) {
        if (a > b) {
            return a;
        }
        return b;
    }
}"#;

        if std::fs::write(&src_file, java_code).is_ok() {
            let status = std::process::Command::new("javac")
                .arg("-g")
                .arg(&src_file)
                .status();

            if let Ok(st) = status {
                if st.success() && class_file.exists() {
                    let bytes = std::fs::read(&class_file).expect("read compiled class");
                    let decompiled = decompile_class_bytes(&bytes).expect("decompile class bytes");

                    assert!(decompiled.contains("public class Service {"));
                    assert!(decompiled.contains("this.counter = init;"), "Got: {}", decompiled);
                    assert!(decompiled.contains("return this.counter + step;"), "Got: {}", decompiled);
                    assert!(decompiled.contains("if (a > b) {"), "Got: {}", decompiled);
                    assert!(decompiled.contains("return a;"), "Got: {}", decompiled);
                    assert!(decompiled.contains("return b;"), "Got: {}", decompiled);
                }
            }
        }
    }

    #[test]
    fn test_extract_source_from_jdk() {
        if let Some(src) = extract_source_from_jdk(None, "java.io.File") {
            assert!(src.contains("public class File"));
            assert!(src.contains("createNewFile()"));
        }
        if let Some(src) = extract_source_from_jdk(None, "java.util.ArrayList") {
            assert!(src.contains("public class ArrayList"));
            assert!(src.contains("add("));
        }
    }
}

