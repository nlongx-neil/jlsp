use std::collections::{HashMap, HashSet};
use std::path::Path;
use cafebabe::attributes::{Annotation, AnnotationElementValue, AttributeData, AttributeInfo, CodeData};
use cafebabe::bytecode::{Opcode, PrimitiveArrayType};
use cafebabe::constant_pool::{LiteralConstant, Loadable, ObjectArrayType};
use cafebabe::{ClassAccessFlags, ClassFile, FieldAccessFlags, MethodAccessFlags};
use crate::model::{ClassInfo, ClassKind, TypeSignature};

/// Clean JVM internal class/descriptor name (e.g. "java/lang/String" -> "String", "Ljava/util/List;" -> "List")
pub fn clean_class_name(s: &str) -> String {
    let mut raw = s.trim();
    if let Some(inner) = raw.strip_prefix("BinaryName(\"").and_then(|r| r.strip_suffix("\")")) {
        raw = inner;
    }
    raw = raw.trim_matches('"').trim_matches('\'');
    raw = raw.trim_start_matches('L').trim_end_matches(';').trim_matches('"');
    let simple = raw.split('/').last().unwrap_or(raw);
    let simple = simple.trim_end_matches("\")").trim_end_matches('"').trim_end_matches(')');
    simple.replace('$', ".")
}

fn clean_object_array_type(t: &ObjectArrayType) -> String {
    match t {
        ObjectArrayType::BinaryName(name) => clean_class_name(name),
        ObjectArrayType::ArrayType(desc) => descriptor_to_simple_type(&desc.to_string()),
    }
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

/// Parse and clean JVM generic type signature into Java syntax
pub fn clean_generic_type_string(sig: &str) -> String {
    let mut parser = SignatureParser::new(sig);
    if sig.starts_with('<') {
        let tp = parser.parse_formal_type_parameters();
        if !tp.is_empty() {
            return tp;
        }
    }
    let res = parser.parse_java_type_signature();
    if res.is_empty() {
        clean_class_name(sig)
    } else {
        res
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedClassSignature {
    pub type_params: String,
    pub super_class: Option<String>,
    pub interfaces: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct ParsedMethodSignature {
    pub type_params: String,
    pub param_types: Vec<String>,
    pub return_type: String,
}

pub struct SignatureParser<'a> {
    chars: &'a [u8],
    pos: usize,
}

impl<'a> SignatureParser<'a> {
    pub fn new(input: &'a str) -> Self {
        Self {
            chars: input.as_bytes(),
            pos: 0,
        }
    }

    pub fn is_eof(&self) -> bool {
        self.pos >= self.chars.len()
    }

    pub fn peek(&self) -> Option<u8> {
        self.chars.get(self.pos).copied()
    }

    pub fn next(&mut self) -> Option<u8> {
        if self.pos < self.chars.len() {
            let ch = self.chars[self.pos];
            self.pos += 1;
            Some(ch)
        } else {
            None
        }
    }

    pub fn eat(&mut self, expected: u8) -> bool {
        if self.peek() == Some(expected) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// Parse formal type parameters: `<T:Ljava/lang/Object;U::Ljava/lang/Comparable<TU;>;>`
    /// Produces `<T, U extends Comparable<U>>`
    pub fn parse_formal_type_parameters(&mut self) -> String {
        if !self.eat(b'<') {
            return String::new();
        }

        let mut params = Vec::new();

        while !self.is_eof() && self.peek() != Some(b'>') {
            // 1. Identifier before ':'
            let mut id = String::new();
            while let Some(ch) = self.peek() {
                if ch == b':' || ch == b'>' {
                    break;
                }
                id.push(ch as char);
                self.next();
            }

            if id.is_empty() {
                break;
            }

            // 2. ClassBound starts with ':'
            if !self.eat(b':') {
                params.push(id);
                break;
            }

            let mut bounds = Vec::new();

            // Explicit ClassBound if not immediately followed by ':' or '>'
            if self.peek() != Some(b':') && self.peek() != Some(b'>') {
                let class_bound = self.parse_java_type_signature();
                if !class_bound.is_empty() && class_bound != "Object" && class_bound != "java.lang.Object" {
                    bounds.push(class_bound);
                }
            }

            // Interface bounds (each preceded by ':')
            while self.eat(b':') {
                let iface_bound = self.parse_java_type_signature();
                if !iface_bound.is_empty() {
                    bounds.push(iface_bound);
                }
            }

            if bounds.is_empty() {
                params.push(id);
            } else {
                params.push(format!("{} extends {}", id, bounds.join(" & ")));
            }
        }

        self.eat(b'>');

        if params.is_empty() {
            String::new()
        } else {
            format!("<{}>", params.join(", "))
        }
    }

    /// Parse a single JavaTypeSignature (primitive, array, type variable, or class type)
    pub fn parse_java_type_signature(&mut self) -> String {
        match self.peek() {
            Some(b'B') => { self.next(); "byte".to_string() }
            Some(b'C') => { self.next(); "char".to_string() }
            Some(b'D') => { self.next(); "double".to_string() }
            Some(b'F') => { self.next(); "float".to_string() }
            Some(b'I') => { self.next(); "int".to_string() }
            Some(b'J') => { self.next(); "long".to_string() }
            Some(b'S') => { self.next(); "short".to_string() }
            Some(b'Z') => { self.next(); "boolean".to_string() }
            Some(b'V') => { self.next(); "void".to_string() }
            Some(b'[') => {
                self.next();
                let elem = self.parse_java_type_signature();
                format!("{}[]", elem)
            }
            Some(b'T') => {
                // TypeVariableSignature: T Identifier ;
                self.next();
                let mut id = String::new();
                while let Some(ch) = self.peek() {
                    if ch == b';' {
                        self.next();
                        break;
                    }
                    id.push(ch as char);
                    self.next();
                }
                id
            }
            Some(b'L') => {
                self.parse_class_type_signature()
            }
            _ => {
                if let Some(ch) = self.next() {
                    (ch as char).to_string()
                } else {
                    String::new()
                }
            }
        }
    }

    /// Parse ClassTypeSignature: L [PackageSpecifier] SimpleClassTypeSignature {ClassTypeSignatureSuffix} ;
    pub fn parse_class_type_signature(&mut self) -> String {
        if !self.eat(b'L') {
            return String::new();
        }

        let mut class_path = String::new();
        while let Some(ch) = self.peek() {
            if ch == b'<' || ch == b';' || ch == b'.' {
                break;
            }
            class_path.push(ch as char);
            self.next();
        }

        let raw = class_path.replace('$', ".");
        let simple = raw.split('/').last().unwrap_or(&raw).to_string();
        let mut result = simple;

        // TypeArguments
        if self.eat(b'<') {
            let mut type_args = Vec::new();
            while !self.is_eof() && self.peek() != Some(b'>') {
                if self.eat(b'*') {
                    type_args.push("?".to_string());
                } else if self.eat(b'+') {
                    let bound = self.parse_java_type_signature();
                    type_args.push(format!("? extends {}", bound));
                } else if self.eat(b'-') {
                    let bound = self.parse_java_type_signature();
                    type_args.push(format!("? super {}", bound));
                } else {
                    let arg = self.parse_java_type_signature();
                    if !arg.is_empty() {
                        type_args.push(arg);
                    }
                }
            }
            self.eat(b'>');
            if !type_args.is_empty() {
                result.push_str(&format!("<{}>", type_args.join(", ")));
            }
        }

        // Inner class suffix e.g. .Entry<K, V>
        while self.eat(b'.') {
            let mut inner_name = String::new();
            while let Some(ch) = self.peek() {
                if ch == b'<' || ch == b';' || ch == b'.' {
                    break;
                }
                inner_name.push(ch as char);
                self.next();
            }
            result.push('.');
            result.push_str(&inner_name);

            if self.eat(b'<') {
                let mut type_args = Vec::new();
                while !self.is_eof() && self.peek() != Some(b'>') {
                    if self.eat(b'*') {
                        type_args.push("?".to_string());
                    } else if self.eat(b'+') {
                        let bound = self.parse_java_type_signature();
                        type_args.push(format!("? extends {}", bound));
                    } else if self.eat(b'-') {
                        let bound = self.parse_java_type_signature();
                        type_args.push(format!("? super {}", bound));
                    } else {
                        let arg = self.parse_java_type_signature();
                        if !arg.is_empty() {
                            type_args.push(arg);
                        }
                    }
                }
                self.eat(b'>');
                if !type_args.is_empty() {
                    result.push_str(&format!("<{}>", type_args.join(", ")));
                }
            }
        }

        self.eat(b';');
        result
    }

    /// Parse complete ClassSignature: [FormalTypeParameters] SuperclassSignature { SuperinterfaceSignature }
    pub fn parse_class_signature(&mut self) -> ParsedClassSignature {
        let type_params = if self.peek() == Some(b'<') {
            self.parse_formal_type_parameters()
        } else {
            String::new()
        };

        let super_class = if self.peek() == Some(b'L') {
            let sc = self.parse_class_type_signature();
            if !sc.is_empty() && sc != "Object" && sc != "java.lang.Object" {
                Some(sc)
            } else {
                None
            }
        } else {
            None
        };

        let mut interfaces = Vec::new();
        while self.peek() == Some(b'L') {
            let iface = self.parse_class_type_signature();
            if !iface.is_empty() && iface != "java.lang.annotation.Annotation" && iface != "Annotation" {
                interfaces.push(iface);
            }
        }

        ParsedClassSignature {
            type_params,
            super_class,
            interfaces,
        }
    }

    /// Parse MethodSignature: [FormalTypeParameters] ( { JavaTypeSignature } ) Result { ThrowsSignature }
    pub fn parse_method_signature(&mut self) -> Option<ParsedMethodSignature> {
        let type_params = if self.peek() == Some(b'<') {
            self.parse_formal_type_parameters()
        } else {
            String::new()
        };

        if !self.eat(b'(') {
            return None;
        }

        let mut param_types = Vec::new();
        while !self.is_eof() && self.peek() != Some(b')') {
            let p_ty = self.parse_java_type_signature();
            if p_ty.is_empty() {
                break;
            }
            param_types.push(p_ty);
        }

        if !self.eat(b')') {
            return None;
        }

        let return_type = self.parse_java_type_signature();

        Some(ParsedMethodSignature {
            type_params,
            param_types,
            return_type,
        })
    }
}

/// Format an AnnotationElementValue to Java source code representation
pub fn format_annotation_element_value(val: &AnnotationElementValue) -> String {
    match val {
        AnnotationElementValue::StringConstant(s) => format!("{:?}", s),
        AnnotationElementValue::IntConstant(i) => i.to_string(),
        AnnotationElementValue::LongConstant(l) => format!("{}L", l),
        AnnotationElementValue::FloatConstant(f) => format!("{}f", f),
        AnnotationElementValue::DoubleConstant(d) => d.to_string(),
        AnnotationElementValue::ByteConstant(b) => b.to_string(),
        AnnotationElementValue::ShortConstant(s) => s.to_string(),
        AnnotationElementValue::CharConstant(c) => {
            if let Some(ch) = char::from_u32(*c as u32) {
                format!("'{}'", ch.escape_default())
            } else {
                format!("'{}'", *c as u8 as char)
            }
        }
        AnnotationElementValue::BooleanConstant(b) => {
            if *b != 0 {
                "true".to_string()
            } else {
                "false".to_string()
            }
        }
        AnnotationElementValue::ClassLiteral { class_name } => {
            format!("{}.class", clean_class_name(class_name))
        }
        AnnotationElementValue::EnumConstant { const_name, type_name } => {
            let type_str = format!("{}", type_name);
            let simple_enum = clean_class_name(&type_str);
            format!("{}.{}", simple_enum, const_name)
        }
        AnnotationElementValue::ArrayValue(elements) => {
            if elements.len() == 1 {
                format_annotation_element_value(&elements[0])
            } else {
                let items: Vec<String> = elements.iter().map(format_annotation_element_value).collect();
                format!("{{{}}}", items.join(", "))
            }
        }
        AnnotationElementValue::AnnotationValue(nested) => {
            format_annotation(nested)
        }
    }
}

/// Format an Annotation to Java source code representation (e.g. "@GetMapping(\"/api\")")
pub fn format_annotation(ann: &Annotation) -> String {
    let desc = format!("{}", ann.type_descriptor);
    let name = clean_class_name(&desc);
    if ann.elements.is_empty() {
        format!("@{}", name)
    } else if ann.elements.len() == 1 && ann.elements[0].name == "value" {
        format!("@{}({})", name, format_annotation_element_value(&ann.elements[0].value))
    } else {
        let params: Vec<String> = ann.elements
            .iter()
            .map(|el| format!("{} = {}", el.name, format_annotation_element_value(&el.value)))
            .collect();
        format!("@{}({})", name, params.join(", "))
    }
}

/// Extract all visible and invisible annotations from attribute list
pub fn extract_annotations<'a>(attrs: &'a [AttributeInfo<'a>]) -> Vec<&'a Annotation<'a>> {
    let mut result = Vec::new();
    for attr in attrs {
        match &attr.data {
            AttributeData::RuntimeVisibleAnnotations(list)
            | AttributeData::RuntimeInvisibleAnnotations(list) => {
                for ann in list {
                    result.push(ann);
                }
            }
            _ => {}
        }
    }
    result
}

/// Check if attribute list contains @Deprecated marker
pub fn is_deprecated(attrs: &[AttributeInfo]) -> bool {
    attrs.iter().any(|a| matches!(a.data, AttributeData::Deprecated))
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

fn is_boolean_expr(expr: &str, bool_vars: &HashSet<String>) -> bool {
    let trimmed = expr.trim();
    if bool_vars.contains(trimmed) {
        return true;
    }
    if trimmed == "true" || trimmed == "false" {
        return true;
    }
    if trimmed.starts_with('!') {
        return true;
    }
    if trimmed.contains(" instanceof ") {
        return true;
    }
    if trimmed.contains(" == ")
        || trimmed.contains(" != ")
        || trimmed.contains(" < ")
        || trimmed.contains(" > ")
        || trimmed.contains(" <= ")
        || trimmed.contains(" >= ")
        || trimmed.contains(" && ")
        || trimmed.contains(" || ")
    {
        return true;
    }
    if let Some((_, call)) = trimmed.rsplit_once('.') {
        let call = call.trim();
        let m_name = call.split('(').next().unwrap_or(call);
        if m_name.starts_with("is") && m_name.len() > 2 && m_name.chars().nth(2).map_or(false, |c| c.is_ascii_uppercase()) {
            return true;
        }
        if m_name.starts_with("has") && m_name.len() > 3 && m_name.chars().nth(3).map_or(false, |c| c.is_ascii_uppercase()) {
            return true;
        }
        if m_name.starts_with("can") && m_name.len() > 3 && m_name.chars().nth(3).map_or(false, |c| c.is_ascii_uppercase()) {
            return true;
        }
        if matches!(
            m_name,
            "equals"
                | "equalsIgnoreCase"
                | "contains"
                | "containsKey"
                | "containsValue"
                | "startsWith"
                | "endsWith"
                | "matches"
                | "hasNext"
                | "isEmpty"
                | "isBlank"
        ) {
            return true;
        }
    }
    false
}

fn wrap_additive(s: String) -> String {
    if (s.contains('+') || s.contains('-')) && !s.starts_with('(') {
        format!("({s})")
    } else {
        s
    }
}

fn simplify_compare(expr: String, op: &str, bool_vars: &HashSet<String>) -> String {
    for prefix in ["Long.compare(", "Double.compare(", "Float.compare(", "Integer.compare("] {
        if let Some(inner) = expr.strip_prefix(prefix).and_then(|s| s.strip_suffix(')')) {
            if let Some((a, b)) = inner.split_once(", ") {
                let java_op = match op {
                    "== 0" => "==",
                    "!= 0" => "!=",
                    "> 0" => ">",
                    ">= 0" => ">=",
                    "< 0" => "<",
                    "<= 0" => "<=",
                    _ => op,
                };
                return format!("{a} {java_op} {b}");
            }
        }
    }
    if is_boolean_expr(&expr, bool_vars) {
        return match op {
            "!= 0" => {
                if expr == "true" {
                    "true".into()
                } else if expr == "false" {
                    "false".into()
                } else {
                    expr
                }
            }
            "== 0" => {
                if expr == "true" {
                    "false".into()
                } else if expr == "false" {
                    "true".into()
                } else if let Some(negated) = expr.strip_prefix('!') {
                    let unwrapped = negated.trim();
                    if unwrapped.starts_with('(') && unwrapped.ends_with(')') {
                        unwrapped[1..unwrapped.len() - 1].trim().to_string()
                    } else {
                        unwrapped.to_string()
                    }
                } else if expr.contains(" == ") {
                    expr.replacen(" == ", " != ", 1)
                } else if expr.contains(" != ") {
                    expr.replacen(" != ", " == ", 1)
                } else if expr.contains(" <= ") {
                    expr.replacen(" <= ", " > ", 1)
                } else if expr.contains(" >= ") {
                    expr.replacen(" >= ", " < ", 1)
                } else if expr.contains(" < ") {
                    expr.replacen(" < ", " >= ", 1)
                } else if expr.contains(" > ") {
                    expr.replacen(" > ", " <= ", 1)
                } else if !expr.contains(' ') {
                    format!("!{expr}")
                } else {
                    format!("!({expr})")
                }
            }
            _ => format!("{expr} {op}"),
        };
    }
    match op {
        "!= 0" => {
            if expr == "true" {
                "true".into()
            } else if expr == "false" {
                "false".into()
            } else {
                format!("{expr} != 0")
            }
        }
        "== 0" => {
            if expr == "true" {
                "false".into()
            } else if expr == "false" {
                "true".into()
            } else {
                format!("{expr} == 0")
            }
        }
        _ => format!("{expr} {op}"),
    }
}

fn is_statement_opcode(op: &Opcode) -> bool {
    match op {
        Opcode::Astore(_)
        | Opcode::Istore(_)
        | Opcode::Lstore(_)
        | Opcode::Fstore(_)
        | Opcode::Dstore(_)
        | Opcode::Aastore
        | Opcode::Iastore
        | Opcode::Lastore
        | Opcode::Fastore
        | Opcode::Dastore
        | Opcode::Bastore
        | Opcode::Castore
        | Opcode::Sastore
        | Opcode::Putfield(_)
        | Opcode::Putstatic(_)
        | Opcode::Return
        | Opcode::Ireturn
        | Opcode::Lreturn
        | Opcode::Freturn
        | Opcode::Dreturn
        | Opcode::Areturn
        | Opcode::Athrow
        | Opcode::Iinc(_, _)
        | Opcode::IfIcmpeq(_)
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
        | Opcode::Ifnonnull(_)
        | Opcode::Tableswitch(_)
        | Opcode::Lookupswitch(_)
        | Opcode::Monitorenter
        | Opcode::Monitorexit => true,
        Opcode::Invokevirtual(m)
        | Opcode::Invokestatic(m)
        | Opcode::Invokespecial(m)
        | Opcode::Invokeinterface(m, _) => {
            let (_, ret) = crate::classpath::descriptor::parse_method_descriptor(&m.name_and_type.descriptor);
            ret == TypeSignature::Void
        }
        Opcode::Invokedynamic(d) => {
            let (_, ret) = crate::classpath::descriptor::parse_method_descriptor(&d.name_and_type.descriptor);
            ret == TypeSignature::Void
        }
        _ => false,
    }
}

fn extract_if_cond(op: &Opcode, stack: &mut Vec<String>, bool_vars: &HashSet<String>) -> String {
    match op {
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
            simplify_compare(a, "!= 0", bool_vars)
        }
        Opcode::Ifne(_) => {
            let a = stack.pop().unwrap_or_default();
            simplify_compare(a, "== 0", bool_vars)
        }
        Opcode::Iflt(_) => {
            let a = stack.pop().unwrap_or_default();
            simplify_compare(a, ">= 0", bool_vars)
        }
        Opcode::Ifge(_) => {
            let a = stack.pop().unwrap_or_default();
            simplify_compare(a, "< 0", bool_vars)
        }
        Opcode::Ifgt(_) => {
            let a = stack.pop().unwrap_or_default();
            simplify_compare(a, "<= 0", bool_vars)
        }
        Opcode::Ifle(_) => {
            let a = stack.pop().unwrap_or_default();
            simplify_compare(a, "> 0", bool_vars)
        }
        Opcode::Ifnull(_) => {
            let a = stack.pop().unwrap_or_default();
            format!("{a} != null")
        }
        Opcode::Ifnonnull(_) => {
            let a = stack.pop().unwrap_or_default();
            format!("{a} == null")
        }
        _ => "true".to_string(),
    }
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

    // 1. Extract LocalVariableTable and LocalVariableTypeTable
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
    for attr in &code_data.attributes {
        if let AttributeData::LocalVariableTypeTable(ref entries) = attr.data {
            for entry in entries {
                let var_type = clean_generic_type_string(&entry.signature);
                if let Some((_, t)) = local_vars.get_mut(&entry.index) {
                    *t = var_type;
                }
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

    // 3. Pre-scan forward jumps for if / if-else / ternary structure
    // pc -> (target_pc, is_if_else, else_end_pc)
    let mut forward_if_targets: HashMap<usize, (usize, bool, usize)> = HashMap::new();
    let mut block_closes: HashMap<usize, usize> = HashMap::new(); // target_pc -> count of braces to close
    let mut else_starts: HashMap<usize, usize> = HashMap::new(); // target_pc -> else_end_pc
    let mut ternary_ifs: HashMap<usize, (usize, usize, usize)> = HashMap::new(); // pc -> (goto_pc, target, else_end)

    // Pre-scan exception table for try-catch blocks
    let mut try_starts: HashMap<usize, usize> = HashMap::new();
    let mut catch_handlers: HashMap<usize, Vec<(String, String)>> = HashMap::new();
    for entry in &code_data.exception_table {
        let start = entry.start_pc as usize;
        let handler = entry.handler_pc as usize;
        let ex_type = entry.catch_type.as_ref().map(|c| clean_class_name(c)).unwrap_or_else(|| "Exception".to_string());
        *try_starts.entry(start).or_insert(0) += 1;
        catch_handlers.entry(handler).or_default().push((ex_type, "e".to_string()));
    }
    let mut case_labels: HashMap<usize, Vec<String>> = HashMap::new();

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
            let mut is_ternary = false;
            let mut goto_pc_ternary = 0;
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
                                let branch1 = &opcodes[i + 1..target_idx - 1];
                                if let Some(else_end_idx) = opcodes.iter().position(|&(p, _)| p == else_end) {
                                    let branch2 = &opcodes[target_idx..else_end_idx];
                                    if !branch1.is_empty()
                                        && branch1.len() <= 4
                                        && !branch1.iter().any(|(_, op)| is_statement_opcode(op))
                                        && !branch2.is_empty()
                                        && branch2.len() <= 4
                                        && !branch2.iter().any(|(_, op)| is_statement_opcode(op))
                                    {
                                        is_ternary = true;
                                        goto_pc_ternary = goto_pc;
                                    }
                                }
                            }
                        }
                    }
                }
            }

            if is_ternary {
                ternary_ifs.insert(pc, (goto_pc_ternary, target, else_end));
            } else {
                forward_if_targets.insert(pc, (target, is_if_else, else_end));
                if is_if_else {
                    else_starts.insert(target, else_end);
                    *block_closes.entry(else_end).or_insert(0) += 1;
                } else {
                    *block_closes.entry(target).or_insert(0) += 1;
                }
            }
        }
    }

    // 4. Symbolic stack evaluation & statement emission
    let mut stack: Vec<String> = Vec::new();
    let mut statements: Vec<String> = Vec::new();
    let param_names: HashSet<String> = param_slots
        .iter()
        .filter_map(|s| local_vars.get(s).map(|(n, _)| n.clone()))
        .collect();
    let mut declared_names: HashSet<String> = param_names;
    let boolean_vars: HashSet<String> = local_vars
        .values()
        .filter(|(_, t)| t == "boolean" || t == "Boolean")
        .map(|(n, _)| n.clone())
        .collect();
    let mut indent_level = 0;
    let mut pending_ternary_gotos: HashMap<usize, usize> = HashMap::new(); // goto_pc -> else_end
    let mut active_ternaries: HashMap<usize, (String, String)> = HashMap::new(); // else_end -> (cond, then_val)

    let format_stmt = |stmt: &str, level: usize| -> String {
        let prefix = "    ".repeat(level);
        format!("{}{}", prefix, stmt)
    };

    for &(pc, ref op) in opcodes {
        // Check if any ternary expression finishes at this pc
        if let Some((cond, then_val)) = active_ternaries.remove(&pc) {
            let else_val = stack.pop().unwrap_or_default();
            stack.push(format!("{cond} ? {then_val} : {else_val}"));
        }
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

        // Check if any try block starts at this pc
        if let Some(count) = try_starts.remove(&pc) {
            for _ in 0..count {
                statements.push(format_stmt("try {", indent_level));
                indent_level += 1;
            }
        }

        // Check if any catch handler starts at this pc
        if let Some(handlers) = catch_handlers.remove(&pc) {
            if indent_level > 0 {
                indent_level -= 1;
            }
            for (ex_type, ex_var) in handlers {
                statements.push(format_stmt(&format!("}} catch ({} {}) {{", ex_type, ex_var), indent_level));
                indent_level += 1;
                stack.push(ex_var);
            }
        }

        // Check if any case / default labels are at this pc
        if let Some(labels) = case_labels.remove(&pc) {
            for lbl in labels {
                statements.push(format_stmt(&lbl, indent_level));
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
                let mut val = stack.pop().unwrap_or_else(|| "null".into());
                let var_name = get_var_name(*slot, &local_vars);
                let var_type = get_var_type(*slot, &local_vars);
                if var_type == "boolean" {
                    if val == "1" {
                        val = "true".to_string();
                    } else if val == "0" {
                        val = "false".to_string();
                    }
                }
                if declared_names.insert(var_name.clone()) {
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
                let mut val = stack.pop().unwrap_or_else(|| "null".into());
                let obj = stack.pop().unwrap_or_else(|| "this".into());
                if member.name_and_type.descriptor == "Z" {
                    if val == "1" {
                        val = "true".to_string();
                    } else if val == "0" {
                        val = "false".to_string();
                    }
                }
                statements.push(format_stmt(&format!("{obj}.{} = {val};", member.name_and_type.name), indent_level));
            }

            Opcode::Putstatic(member) => {
                let mut val = stack.pop().unwrap_or_else(|| "null".into());
                let owner = clean_class_name(&member.class_name);
                if member.name_and_type.descriptor == "Z" {
                    if val == "1" {
                        val = "true".to_string();
                    } else if val == "0" {
                        val = "false".to_string();
                    }
                }
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
                for (idx, arg) in args.iter_mut().enumerate() {
                    if idx < arg_types.len() && arg_types[idx] == TypeSignature::Boolean {
                        if arg == "1" {
                            *arg = "true".to_string();
                        } else if arg == "0" {
                            *arg = "false".to_string();
                        }
                    }
                }

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
                    let receiver_str = if receiver.starts_with('(') && !receiver.ends_with(')') {
                        format!("({receiver})")
                    } else {
                        receiver
                    };
                    let call = format!("{receiver_str}.{}({})", member.name_and_type.name, args.join(", "));
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
                for (idx, arg) in args.iter_mut().enumerate() {
                    if idx < arg_types.len() && arg_types[idx] == TypeSignature::Boolean {
                        if arg == "1" {
                            *arg = "true".to_string();
                        } else if arg == "0" {
                            *arg = "false".to_string();
                        }
                    }
                }

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
                    let receiver_str = if receiver.starts_with('(') && !receiver.ends_with(')') {
                        format!("({receiver})")
                    } else {
                        receiver
                    };
                    let call = format!("{receiver_str}.{m_name}({})", args.join(", "));
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
                for (idx, arg) in args.iter_mut().enumerate() {
                    if idx < arg_types.len() && arg_types[idx] == TypeSignature::Boolean {
                        if arg == "1" {
                            *arg = "true".to_string();
                        } else if arg == "0" {
                            *arg = "false".to_string();
                        }
                    }
                }

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
                    if args.is_empty() {
                        stack.push("\"\"".into());
                    } else {
                        stack.push(args.join(" + "));
                    }
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
                let b = wrap_additive(stack.pop().unwrap_or_default());
                let a = wrap_additive(stack.pop().unwrap_or_default());
                stack.push(format!("{a} * {b}"));
            }
            Opcode::Idiv | Opcode::Ldiv | Opcode::Fdiv | Opcode::Ddiv => {
                let b = wrap_additive(stack.pop().unwrap_or_default());
                let a = wrap_additive(stack.pop().unwrap_or_default());
                stack.push(format!("{a} / {b}"));
            }
            Opcode::Irem | Opcode::Lrem | Opcode::Frem | Opcode::Drem => {
                let b = wrap_additive(stack.pop().unwrap_or_default());
                let a = wrap_additive(stack.pop().unwrap_or_default());
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
                let el = clean_object_array_type(elem);
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
                let target_name = clean_object_array_type(target);
                stack.push(format!("({target_name}) {val}"));
            }

            Opcode::Instanceof(target) => {
                let val = stack.pop().unwrap_or_default();
                let target_name = clean_object_array_type(target);
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
                if let Some(&(goto_pc, _target, else_end)) = ternary_ifs.get(&pc) {
                    let cond = extract_if_cond(op, &mut stack, &boolean_vars);
                    pending_ternary_gotos.insert(goto_pc, else_end);
                    active_ternaries.insert(else_end, (cond, String::new()));
                    continue;
                }
                let cond = extract_if_cond(op, &mut stack, &boolean_vars);
                statements.push(format_stmt(&format!("if ({cond}) {{"), indent_level));
                indent_level += 1;
            }

            Opcode::Goto(offset) => {
                if let Some(&else_end) = pending_ternary_gotos.get(&pc) {
                    let then_val = stack.pop().unwrap_or_default();
                    if let Some(entry) = active_ternaries.get_mut(&else_end) {
                        entry.1 = then_val;
                    }
                    continue;
                }
                let target = (pc as i32 + offset) as usize;
                if target < pc {
                    // Backward jump = loop!
                    statements.push(format_stmt("continue;", indent_level));
                }
            }

            Opcode::I2l => {
                let a = stack.pop().unwrap_or_else(|| "0".into());
                stack.push(format!("(long) {a}"));
            }
            Opcode::I2f => {
                let a = stack.pop().unwrap_or_else(|| "0".into());
                stack.push(format!("(float) {a}"));
            }
            Opcode::I2d => {
                let a = stack.pop().unwrap_or_else(|| "0".into());
                stack.push(format!("(double) {a}"));
            }
            Opcode::L2i => {
                let a = stack.pop().unwrap_or_else(|| "0L".into());
                stack.push(format!("(int) {a}"));
            }
            Opcode::L2f => {
                let a = stack.pop().unwrap_or_else(|| "0L".into());
                stack.push(format!("(float) {a}"));
            }
            Opcode::L2d => {
                let a = stack.pop().unwrap_or_else(|| "0L".into());
                stack.push(format!("(double) {a}"));
            }
            Opcode::F2i => {
                let a = stack.pop().unwrap_or_else(|| "0.0f".into());
                stack.push(format!("(int) {a}"));
            }
            Opcode::F2l => {
                let a = stack.pop().unwrap_or_else(|| "0.0f".into());
                stack.push(format!("(long) {a}"));
            }
            Opcode::F2d => {
                let a = stack.pop().unwrap_or_else(|| "0.0f".into());
                stack.push(format!("(double) {a}"));
            }
            Opcode::D2i => {
                let a = stack.pop().unwrap_or_else(|| "0.0".into());
                stack.push(format!("(int) {a}"));
            }
            Opcode::D2l => {
                let a = stack.pop().unwrap_or_else(|| "0.0".into());
                stack.push(format!("(long) {a}"));
            }
            Opcode::D2f => {
                let a = stack.pop().unwrap_or_else(|| "0.0".into());
                stack.push(format!("(float) {a}"));
            }
            Opcode::I2b => {
                let a = stack.pop().unwrap_or_else(|| "0".into());
                stack.push(format!("(byte) {a}"));
            }
            Opcode::I2c => {
                let a = stack.pop().unwrap_or_else(|| "0".into());
                stack.push(format!("(char) {a}"));
            }
            Opcode::I2s => {
                let a = stack.pop().unwrap_or_else(|| "0".into());
                stack.push(format!("(short) {a}"));
            }

            Opcode::Lcmp => {
                let b = stack.pop().unwrap_or_else(|| "0L".into());
                let a = stack.pop().unwrap_or_else(|| "0L".into());
                stack.push(format!("Long.compare({a}, {b})"));
            }
            Opcode::Fcmpl | Opcode::Fcmpg => {
                let b = stack.pop().unwrap_or_else(|| "0.0f".into());
                let a = stack.pop().unwrap_or_else(|| "0.0f".into());
                stack.push(format!("Float.compare({a}, {b})"));
            }
            Opcode::Dcmpl | Opcode::Dcmpg => {
                let b = stack.pop().unwrap_or_else(|| "0.0".into());
                let a = stack.pop().unwrap_or_else(|| "0.0".into());
                stack.push(format!("Double.compare({a}, {b})"));
            }

            Opcode::Tableswitch(range_table) => {
                let selector = stack.pop().unwrap_or_else(|| "key".into());
                statements.push(format_stmt(&format!("switch ({selector}) {{"), indent_level));
                indent_level += 1;
                for (idx, jump) in range_table.jumps.iter().enumerate() {
                    let case_val = range_table.low + idx as i32;
                    let target = (pc as i32 + jump) as usize;
                    case_labels.entry(target).or_default().push(format!("case {}:", case_val));
                }
                let def_target = (pc as i32 + range_table.default) as usize;
                case_labels.entry(def_target).or_default().push("default:".to_string());
            }

            Opcode::Lookupswitch(lookup_table) => {
                let selector = stack.pop().unwrap_or_else(|| "key".into());
                statements.push(format_stmt(&format!("switch ({selector}) {{"), indent_level));
                indent_level += 1;
                for &(case_val, jump) in &lookup_table.match_offsets {
                    let target = (pc as i32 + jump) as usize;
                    case_labels.entry(target).or_default().push(format!("case {}:", case_val));
                }
                let def_target = (pc as i32 + lookup_table.default) as usize;
                case_labels.entry(def_target).or_default().push("default:".to_string());
            }

            Opcode::Monitorenter => {
                let obj = stack.pop().unwrap_or_else(|| "this".into());
                statements.push(format_stmt(&format!("synchronized ({obj}) {{"), indent_level));
                indent_level += 1;
            }
            Opcode::Monitorexit => {
                let _ = stack.pop();
                if indent_level > 0 {
                    indent_level -= 1;
                }
                statements.push(format_stmt("}", indent_level));
            }

            Opcode::Dup2 => {
                if let Some(top) = stack.last().cloned() {
                    stack.push(top);
                }
            }
            Opcode::DupX1 => {
                if stack.len() >= 2 {
                    let v1 = stack.pop().unwrap();
                    let v2 = stack.pop().unwrap();
                    stack.push(v1.clone());
                    stack.push(v2);
                    stack.push(v1);
                }
            }
            Opcode::DupX2 => {
                if stack.len() >= 3 {
                    let v1 = stack.pop().unwrap();
                    let v2 = stack.pop().unwrap();
                    let v3 = stack.pop().unwrap();
                    stack.push(v1.clone());
                    stack.push(v3);
                    stack.push(v2);
                    stack.push(v1);
                }
            }
            Opcode::Swap => {
                if stack.len() >= 2 {
                    let a = stack.pop().unwrap();
                    let b = stack.pop().unwrap();
                    stack.push(a);
                    stack.push(b);
                }
            }
            Opcode::Nop => {}

            Opcode::Multianewarray(elem, dims) => {
                let mut dim_exprs = Vec::new();
                for _ in 0..*dims {
                    if let Some(d) = stack.pop() {
                        dim_exprs.push(d);
                    }
                }
                dim_exprs.reverse();
                let el = clean_object_array_type(elem);
                let dims_str = dim_exprs.into_iter().map(|d| format!("[{d}]")).collect::<String>();
                stack.push(format!("new {el}{dims_str}"));
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
                let kw = if cls.kind == ClassKind::Interface { "extends" } else { "implements" };
                out.push_str(&format!(" {} {}", kw, ifaces.join(", ")));
            }
        }
    }

    out.push_str(" {\n");

    // 3. Fields
    for field in &cls.fields {
        let vis = if field.is_public { "    public " } else { "    " };
        let stat = if field.is_static { "static " } else { "" };
        let fin = if field.is_final { "final " } else { "" };
        out.push_str(&format!(
            "{}{}{}{}{};\n",
            vis,
            stat,
            fin,
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

/// Decompile a parsed cafebabe::ClassFile directly into human-readable Java source
pub fn decompile_class_file(class_file: &ClassFile) -> String {
    let mut out = String::new();

    let this_class_raw = class_file.this_class.replace('/', ".");
    let (package_name, simple_name) = match this_class_raw.rfind('.') {
        Some(idx) => (this_class_raw[..idx].to_string(), this_class_raw[idx + 1..].to_string()),
        None => (String::new(), this_class_raw.clone()),
    };

    // 1. Package statement
    if !package_name.is_empty() {
        out.push_str(&format!("package {};\n\n", package_name));
    }

    out.push_str("// [READ-ONLY] Decompiled by jlsp (pure Rust, JVM-free). DO NOT EDIT.\n");

    let is_interface = class_file.access_flags.contains(ClassAccessFlags::INTERFACE);
    let is_enum = class_file.access_flags.contains(ClassAccessFlags::ENUM);
    let is_annotation = class_file.access_flags.contains(ClassAccessFlags::ANNOTATION);
    let is_record = class_file.super_class.as_deref() == Some("java/lang/Record")
        || class_file.attributes.iter().any(|a| matches!(a.data, AttributeData::Record(_)));

    // 2. Collect imports from class annotations, members, superclass, interfaces
    let mut imports = HashSet::new();
    let collect_fqn = |desc: &str, set: &mut HashSet<String>| {
        let clean = desc.trim_start_matches('[').trim_start_matches('L').trim_end_matches(';');
        if clean.contains('/') {
            let fqn = clean.replace('/', ".");
            if !fqn.starts_with("java.lang.") && !fqn.starts_with(&format!("{}.", package_name)) {
                set.insert(fqn);
            }
        }
    };

    for ann in extract_annotations(&class_file.attributes) {
        collect_fqn(&format!("{}", ann.type_descriptor), &mut imports);
    }
    for f in &class_file.fields {
        for ann in extract_annotations(&f.attributes) {
            collect_fqn(&format!("{}", ann.type_descriptor), &mut imports);
        }
        collect_fqn(&format!("{}", f.descriptor), &mut imports);
    }
    for m in &class_file.methods {
        for ann in extract_annotations(&m.attributes) {
            collect_fqn(&format!("{}", ann.type_descriptor), &mut imports);
        }
        for attr in &m.attributes {
            if let AttributeData::Exceptions(ref list) = attr.data {
                for ex in list {
                    collect_fqn(ex, &mut imports);
                }
            }
        }
    }

    let mut sorted_imports: Vec<String> = imports.into_iter().collect();
    sorted_imports.sort();
    if !sorted_imports.is_empty() {
        for imp in sorted_imports {
            out.push_str(&format!("import {};\n", imp));
        }
        out.push('\n');
    }

    // 3. Class-level annotations
    let class_annotations = extract_annotations(&class_file.attributes);
    for ann in &class_annotations {
        out.push_str(&format!("{}\n", format_annotation(ann)));
    }
    if is_deprecated(&class_file.attributes) && !class_annotations.iter().any(|a| format!("{}", a.type_descriptor).contains("Deprecated")) {
        out.push_str("@Deprecated\n");
    }

    // 4. Class header
    let kind_str = if is_annotation {
        "@interface"
    } else if is_interface {
        "interface"
    } else if is_enum {
        "enum"
    } else if is_record {
        "record"
    } else {
        "class"
    };

    let mut class_mods = Vec::new();
    if class_file.access_flags.contains(ClassAccessFlags::PUBLIC) {
        class_mods.push("public");
    }
    if class_file.access_flags.contains(ClassAccessFlags::ABSTRACT) && !is_interface && !is_annotation {
        class_mods.push("abstract");
    }
    if class_file.access_flags.contains(ClassAccessFlags::FINAL) && !is_enum && !is_record {
        class_mods.push("final");
    }

    let mods_prefix = if class_mods.is_empty() {
        String::new()
    } else {
        format!("{} ", class_mods.join(" "))
    };

    // Generic signature if present
    let class_sig_opt = class_file.attributes.iter().find_map(|a| {
        if let AttributeData::Signature(ref sig) = a.data {
            let mut parser = SignatureParser::new(sig);
            Some(parser.parse_class_signature())
        } else {
            None
        }
    });

    let type_params = class_sig_opt
        .as_ref()
        .map(|s| s.type_params.clone())
        .unwrap_or_default();

    out.push_str(&format!("{}{}{} {}{}", mods_prefix, kind_str, if kind_str.is_empty() { "" } else { "" }, simple_name, type_params));

    if !is_annotation && !is_enum && !is_record {
        let super_class_str = if let Some(ref cs) = class_sig_opt {
            cs.super_class.clone()
        } else if let Some(ref super_cls) = class_file.super_class {
            let super_clean = super_cls.replace('/', ".");
            if super_clean != "java.lang.Object" {
                Some(super_clean.split('.').last().unwrap_or(&super_clean).to_string())
            } else {
                None
            }
        } else {
            None
        };

        if let Some(ref sc) = super_class_str {
            out.push_str(&format!(" extends {}", sc));
        }

        let ifaces: Vec<String> = if let Some(ref cs) = class_sig_opt {
            if !cs.interfaces.is_empty() {
                cs.interfaces.clone()
            } else {
                class_file
                    .interfaces
                    .iter()
                    .filter(|i| format!("{}", i) != "java/lang/annotation/Annotation")
                    .map(|i| clean_class_name(i))
                    .collect()
            }
        } else {
            class_file
                .interfaces
                .iter()
                .filter(|i| format!("{}", i) != "java/lang/annotation/Annotation")
                .map(|i| clean_class_name(i))
                .collect()
        };

        if !ifaces.is_empty() {
            let kw = if is_interface { "extends" } else { "implements" };
            out.push_str(&format!(" {} {}", kw, ifaces.join(", ")));
        }

        // Permits (sealed classes)
        for attr in &class_file.attributes {
            if let AttributeData::PermittedSubclasses(ref list) = attr.data {
                let subs: Vec<String> = list.iter().map(|s| clean_class_name(s)).collect();
                if !subs.is_empty() {
                    out.push_str(&format!(" permits {}", subs.join(", ")));
                }
            }
        }
    }

    out.push_str(" {\n");

    // 5. Enum constants (if enum)
    if is_enum {
        let mut enum_constants = Vec::new();
        for f in &class_file.fields {
            if f.access_flags.contains(FieldAccessFlags::STATIC)
                && f.access_flags.contains(FieldAccessFlags::FINAL)
                && f.access_flags.contains(FieldAccessFlags::ENUM) {
                enum_constants.push(f.name.to_string());
            }
        }
        if !enum_constants.is_empty() {
            out.push_str(&format!("    {};\n\n", enum_constants.join(", ")));
        }
    }

    // 6. Fields
    for f in &class_file.fields {
        // Skip enum constants fields already emitted
        if is_enum && f.access_flags.contains(FieldAccessFlags::ENUM) {
            continue;
        }

        // Field annotations
        for ann in extract_annotations(&f.attributes) {
            out.push_str(&format!("    {}\n", format_annotation(ann)));
        }
        if is_deprecated(&f.attributes) {
            out.push_str("    @Deprecated\n");
        }

        let mut f_mods = Vec::new();
        if f.access_flags.contains(FieldAccessFlags::PUBLIC) {
            f_mods.push("public");
        } else if f.access_flags.contains(FieldAccessFlags::PROTECTED) {
            f_mods.push("protected");
        } else if f.access_flags.contains(FieldAccessFlags::PRIVATE) {
            f_mods.push("private");
        }
        if f.access_flags.contains(FieldAccessFlags::STATIC) {
            f_mods.push("static");
        }
        if f.access_flags.contains(FieldAccessFlags::FINAL) {
            f_mods.push("final");
        }
        if f.access_flags.contains(FieldAccessFlags::VOLATILE) {
            f_mods.push("volatile");
        }
        if f.access_flags.contains(FieldAccessFlags::TRANSIENT) {
            f_mods.push("transient");
        }

        let f_mod_str = if f_mods.is_empty() {
            String::new()
        } else {
            format!("{} ", f_mods.join(" "))
        };

        // Type: check generic signature attribute first
        let f_type = f.attributes.iter().find_map(|a| {
            if let AttributeData::Signature(ref sig) = a.data {
                Some(clean_generic_type_string(sig))
            } else {
                None
            }
        }).unwrap_or_else(|| descriptor_to_simple_type(&format!("{}", f.descriptor)));

        // Constant value if present
        let mut const_val_str = String::new();
        for attr in &f.attributes {
            if let AttributeData::ConstantValue(ref cv) = attr.data {
                match cv {
                    LiteralConstant::String(s) => const_val_str = format!(" = {:?}", s),
                    LiteralConstant::StringBytes(b) => const_val_str = format!(" = {:?}", String::from_utf8_lossy(b)),
                    LiteralConstant::Integer(i) => const_val_str = format!(" = {}", i),
                    LiteralConstant::Long(l) => const_val_str = format!(" = {}L", l),
                    LiteralConstant::Float(f) => const_val_str = format!(" = {}f", f),
                    LiteralConstant::Double(d) => const_val_str = format!(" = {}", d),
                }
            }
        }

        out.push_str(&format!("    {}{}{} {}{};\n", f_mod_str, f_type, if f_type.is_empty() { "" } else { "" }, f.name, const_val_str));
    }

    if !class_file.fields.is_empty() && !class_file.methods.is_empty() {
        out.push('\n');
    }

    // 7. Methods & Constructors
    for m in &class_file.methods {
        // Skip synthetic methods and static initializers unless needed
        if m.name == "<clinit>" || m.access_flags.contains(MethodAccessFlags::SYNTHETIC) {
            continue;
        }

        // Method annotations
        for ann in extract_annotations(&m.attributes) {
            out.push_str(&format!("    {}\n", format_annotation(ann)));
        }
        if is_deprecated(&m.attributes) {
            out.push_str("    @Deprecated\n");
        }

        let is_constructor = m.name == "<init>";
        let is_abstract = m.access_flags.contains(MethodAccessFlags::ABSTRACT);
        let is_native = m.access_flags.contains(MethodAccessFlags::NATIVE);
        let is_static = m.access_flags.contains(MethodAccessFlags::STATIC);

        let mut m_mods = Vec::new();
        if m.access_flags.contains(MethodAccessFlags::PUBLIC) {
            m_mods.push("public");
        } else if m.access_flags.contains(MethodAccessFlags::PROTECTED) {
            m_mods.push("protected");
        } else if m.access_flags.contains(MethodAccessFlags::PRIVATE) {
            m_mods.push("private");
        }
        if is_static {
            m_mods.push("static");
        }
        if m.access_flags.contains(MethodAccessFlags::FINAL) && !is_interface {
            m_mods.push("final");
        }
        if m.access_flags.contains(MethodAccessFlags::SYNCHRONIZED) {
            m_mods.push("synchronized");
        }
        if is_native {
            m_mods.push("native");
        }
        if is_abstract && !is_interface {
            m_mods.push("abstract");
        }

        let desc_str = format!("{}", m.descriptor);
        let (param_types, ret_type) = crate::classpath::descriptor::parse_method_descriptor(&desc_str);

        // Method signature: check for generic MethodSignature attribute
        let method_sig_opt = m.attributes.iter().find_map(|a| {
            if let AttributeData::Signature(ref sig) = a.data {
                let mut parser = SignatureParser::new(sig);
                parser.parse_method_signature()
            } else {
                None
            }
        });

        let (param_types_str, ret_type_str, method_type_params) = if let Some(ref msig) = method_sig_opt {
            let p_types = if msig.param_types.len() == param_types.len() {
                msig.param_types.clone()
            } else {
                param_types.iter().map(|p| p.simple_name()).collect()
            };
            let ret = if is_constructor {
                String::new()
            } else if !msig.return_type.is_empty() {
                msig.return_type.clone()
            } else {
                ret_type.simple_name()
            };
            let tp = if msig.type_params.is_empty() {
                String::new()
            } else {
                format!("{} ", msig.type_params)
            };
            (p_types, ret, tp)
        } else {
            (
                param_types.iter().map(|p| p.simple_name()).collect(),
                if is_constructor { String::new() } else { ret_type.simple_name() },
                String::new(),
            )
        };

        let mut code_data_opt = None;
        let mut local_param_names = HashMap::new();
        let mut annotation_default = None;

        for attr in &m.attributes {
            match &attr.data {
                AttributeData::Code(cd) => {
                    code_data_opt = Some(cd);
                    for c_attr in &cd.attributes {
                        if let AttributeData::LocalVariableTable(ref entries) = c_attr.data {
                            let mut slot = if is_static { 0 } else { 1 };
                            for (_idx, pty) in param_types.iter().enumerate() {
                                if let Some(e) = entries.iter().find(|e| e.index == slot && e.start_pc == 0) {
                                    local_param_names.insert(slot, e.name.to_string());
                                }
                                if matches!(pty, TypeSignature::Long | TypeSignature::Double) {
                                    slot += 2;
                                } else {
                                    slot += 1;
                                }
                            }
                        }
                    }
                }
                AttributeData::AnnotationDefault(val) => {
                    annotation_default = Some(format_annotation_element_value(val));
                }
                _ => {}
            }
        }

        // Method parameter annotations
        let mut param_annotations_map: HashMap<usize, Vec<String>> = HashMap::new();
        for attr in &m.attributes {
            if let AttributeData::RuntimeVisibleParameterAnnotations(ref p_anns) = attr.data {
                for (p_idx, p_ann) in p_anns.iter().enumerate() {
                    for ann in &p_ann.annotations {
                        param_annotations_map.entry(p_idx).or_default().push(format_annotation(ann));
                    }
                }
            }
        }

        // Check if interface default method
        if is_interface && code_data_opt.is_some() && !is_static {
            m_mods.push("default");
        }

        let m_mod_str = if m_mods.is_empty() {
            String::new()
        } else {
            format!("{} ", m_mods.join(" "))
        };

        // Parameters
        let mut param_strs = Vec::new();
        let mut slot = if is_static { 0 } else { 1 };
        for (idx, p_type_name) in param_types_str.iter().enumerate() {
            let p_name = local_param_names.get(&slot).cloned().unwrap_or_else(|| format!("arg{idx}"));
            let p_ann_str = if let Some(anns) = param_annotations_map.get(&idx) {
                format!("{} ", anns.join(" "))
            } else {
                String::new()
            };
            param_strs.push(format!("{}{}{} {}", p_ann_str, p_type_name, if p_type_name.is_empty() { "" } else { "" }, p_name));
            if let Some(pty) = param_types.get(idx) {
                if matches!(pty, TypeSignature::Long | TypeSignature::Double) {
                    slot += 2;
                } else {
                    slot += 1;
                }
            } else {
                slot += 1;
            }
        }

        // Throws list
        let mut throws_list = Vec::new();
        for attr in &m.attributes {
            if let AttributeData::Exceptions(ref list) = attr.data {
                for ex in list {
                    throws_list.push(clean_class_name(ex));
                }
            }
        }
        let throws_str = if throws_list.is_empty() {
            String::new()
        } else {
            format!(" throws {}", throws_list.join(", "))
        };

        let ret_prefix = if ret_type_str.is_empty() {
            String::new()
        } else {
            format!("{} ", ret_type_str)
        };

        if is_annotation {
            let def_str = if let Some(d) = annotation_default {
                format!(" default {}", d)
            } else {
                String::new()
            };
            out.push_str(&format!(
                "    {}{}{}{}(){}{};\n",
                m_mod_str,
                method_type_params,
                ret_prefix,
                m.name,
                def_str,
                throws_str
            ));
        } else if is_interface && code_data_opt.is_none() {
            out.push_str(&format!(
                "    {}{}{}{}({}){};\n",
                m_mod_str,
                method_type_params,
                ret_prefix,
                m.name,
                param_strs.join(", "),
                throws_str
            ));
        } else if is_abstract || is_native {
            out.push_str(&format!(
                "    {}{}{}{}({}){};\n",
                m_mod_str,
                method_type_params,
                ret_prefix,
                m.name,
                param_strs.join(", "),
                throws_str
            ));
        } else if let Some(cd) = code_data_opt {
            let decompiled = decompile_code_data(
                &m.name,
                &desc_str,
                is_static,
                is_constructor,
                &simple_name,
                cd,
            ).unwrap_or_default();

            let method_ident = if is_constructor {
                simple_name.clone()
            } else {
                format!("{}{}", ret_prefix, m.name)
            };

            if decompiled.trim().is_empty() {
                out.push_str(&format!(
                    "    {}{}{}({}){} {{\n    }}\n",
                    m_mod_str,
                    method_type_params,
                    method_ident,
                    param_strs.join(", "),
                    throws_str
                ));
            } else {
                let indented = indent_lines(decompiled.trim(), "        ");
                out.push_str(&format!(
                    "    {}{}{}({}){} {{\n{}\n    }}\n",
                    m_mod_str,
                    method_type_params,
                    method_ident,
                    param_strs.join(", "),
                    throws_str,
                    indented
                ));
            }
        } else {
            let method_ident = if is_constructor {
                simple_name.clone()
            } else {
                format!("{}{}", ret_prefix, m.name)
            };
            out.push_str(&format!(
                "    {}{}{}({}){} {{ /* compiled code */ }}\n",
                m_mod_str,
                method_type_params,
                method_ident,
                param_strs.join(", "),
                throws_str
            ));
        }
    }

    out.push_str("}\n");
    out
}

/// Decompile a class directly from raw .class file bytes
pub fn decompile_class_bytes(bytes: &[u8]) -> Result<String, String> {
    if let Ok(class_file) = cafebabe::parse_class(bytes) {
        return Ok(decompile_class_file(&class_file));
    }
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
        let home = std::env::home_dir()?;
        let artifact_part = file_name.strip_suffix(".jar")?;
        let expected_name = format!("{}-sources.jar", artifact_part);

        let search_dirs = [
            crate::project::maven::MavenProject::find_m2_repository(),
            Some(home.join(".gradle").join("caches").join("modules-2").join("files-2.1")),
        ];
        for base in search_dirs.into_iter().flatten() {
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
        candidate_src_zips.push(home.join(".sdkman").join("candidates").join("java").join("current").join("lib").join("src.zip"));
        candidate_src_zips.push(home.join(".sdkman").join("candidates").join("java").join("current").join("src.zip"));
    }

    #[cfg(target_os = "macos")]
    {
        let jvm_dir = Path::new("/Library/Java/JavaVirtualMachines");
        if jvm_dir.exists() {
            if let Ok(entries) = std::fs::read_dir(jvm_dir) {
                for entry in entries.flatten() {
                    candidate_src_zips.push(entry.path().join("Contents").join("Home").join("lib").join("src.zip"));
                    candidate_src_zips.push(entry.path().join("Contents").join("Home").join("src.zip"));
                }
            }
        }
    }

    let clean_fqn = fqn.split('$').next().unwrap_or(fqn);
    let rel_path = format!("{}.java", clean_fqn.replace('.', "/"));
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

    #[test]
    fn test_decompile_real_bytecode_with_switch_try_catch_and_annotations() {
        let temp_dir = std::env::temp_dir().join("jlsp_real_decomp_advanced");
        let _ = std::fs::create_dir_all(&temp_dir);
        let src_file = temp_dir.join("AdvancedService.java");
        let class_file = temp_dir.join("AdvancedService.class");

        let java_code = r#"import java.io.IOException;

@Deprecated
public class AdvancedService {
    public static final int MAX_RETRIES = 5;

    public int testSwitch(int type) {
        switch (type) {
            case 1:
                return 100;
            case 2:
                return 200;
            default:
                return 0;
        }
    }

    public void testTryCatch() {
        try {
            System.out.println("try");
        } catch (Exception e) {
            e.printStackTrace();
        }
    }

    public synchronized void testSync(Object lock) {
        synchronized (lock) {
            System.out.println("sync");
        }
    }

    public long testCast(int val) {
        return (long) val;
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

                    assert!(decompiled.contains("public class AdvancedService {"));
                    assert!(decompiled.contains("@Deprecated"));
                    assert!(decompiled.contains("public static final int MAX_RETRIES = 5;"));
                    assert!(decompiled.contains("switch (") && decompiled.contains("case 1:"));
                    assert!(decompiled.contains("try {") && decompiled.contains("catch ("));
                    assert!(decompiled.contains("synchronized (lock) {"));
                    assert!(decompiled.contains("(long)"));
                }
            }
        }
    }

    #[test]
    fn test_generic_signature_parsing() {
        // Formal type parameters
        assert_eq!(
            SignatureParser::new("<T:Ljava/lang/Object;>").parse_formal_type_parameters(),
            "<T>"
        );
        assert_eq!(
            SignatureParser::new("<T::Ljava/lang/Comparable<TT;>;>").parse_formal_type_parameters(),
            "<T extends Comparable<T>>"
        );
        assert_eq!(
            SignatureParser::new("<K:Ljava/lang/Object;V:Ljava/lang/Object;>").parse_formal_type_parameters(),
            "<K, V>"
        );
        assert_eq!(
            SignatureParser::new("<E:Ljava/lang/Number;:Ljava/io/Serializable;>").parse_formal_type_parameters(),
            "<E extends Number & Serializable>"
        );

        // Class signature
        let cls_sig = SignatureParser::new("<T:Ljava/lang/Object;>Ljava/lang/Object;").parse_class_signature();
        assert_eq!(cls_sig.type_params, "<T>");
        assert_eq!(cls_sig.super_class, None);
        assert_eq!(cls_sig.interfaces, Vec::<String>::new());

        let cls_sig2 = SignatureParser::new("<E:Ljava/lang/Object;>Ljava/util/ArrayList<TE;>;Ljava/util/List<TE;>;").parse_class_signature();
        assert_eq!(cls_sig2.type_params, "<E>");
        assert_eq!(cls_sig2.super_class, Some("ArrayList<E>".to_string()));
        assert_eq!(cls_sig2.interfaces, vec!["List<E>".to_string()]);

        // Method signature
        let msig = SignatureParser::new("()TT;").parse_method_signature().unwrap();
        assert_eq!(msig.type_params, "");
        assert_eq!(msig.param_types, Vec::<String>::new());
        assert_eq!(msig.return_type, "T");

        let msig2 = SignatureParser::new("<T:Ljava/lang/Object;>(Ljava/util/List<TT;>;I)TT;").parse_method_signature().unwrap();
        assert_eq!(msig2.type_params, "<T>");
        assert_eq!(msig2.param_types, vec!["List<T>".to_string(), "int".to_string()]);
        assert_eq!(msig2.return_type, "T");

        // Field signature
        assert_eq!(
            clean_generic_type_string("Ljava/util/Map<Ljava/lang/String;Ljava/util/List<TT;>;>;"),
            "Map<String, List<T>>"
        );
    }

    #[test]
    fn test_decompile_instanceof_checkcast_syntax() {
        let temp_dir = std::env::temp_dir().join("jlsp_real_instanceof_test");
        let _ = std::fs::create_dir_all(&temp_dir);
        let src_file = temp_dir.join("MapUtilsHelper.java");
        let class_file = temp_dir.join("MapUtilsHelper.class");

        let java_code = r#"import java.util.Map;

public class MapUtilsHelper {
    public static <K> Boolean getBoolean(Map<? super K, ?> map, K key) {
        if (map != null) {
            Object answer = map.get(key);
            if (answer != null) {
                if (answer instanceof Boolean) {
                    return (Boolean) answer;
                }
                if (answer instanceof String) {
                    return Boolean.valueOf((String) answer);
                }
                if (answer instanceof Number) {
                    Number n = (Number) answer;
                    return n.intValue() != 0 ? Boolean.TRUE : Boolean.FALSE;
                }
            }
        }
        return null;
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

                    // Ensure no stray quotes or BinaryName artefacts
                    assert!(!decompiled.contains("Boolean\""), "Got: {}", decompiled);
                    assert!(!decompiled.contains("String\""), "Got: {}", decompiled);
                    assert!(!decompiled.contains("Number\""), "Got: {}", decompiled);
                    assert!(!decompiled.contains("BinaryName"), "Got: {}", decompiled);

                    // Ensure boolean instanceof check is clean without != 0
                    assert!(decompiled.contains("answer instanceof Boolean"), "Got: {}", decompiled);
                    assert!(!decompiled.contains("instanceof Boolean != 0"), "Got: {}", decompiled);

                    // Ensure cast formatting has balanced parentheses and clean types
                    assert!(decompiled.contains("(Boolean) answer"), "Got: {}", decompiled);
                    assert!(decompiled.contains("(String) answer"), "Got: {}", decompiled);
                    assert!(decompiled.contains("(Number) answer"), "Got: {}", decompiled);
                    assert!(!decompiled.contains("(Boolean))"), "Got: {}", decompiled);
                    assert!(!decompiled.contains("(String))"), "Got: {}", decompiled);
                    assert!(!decompiled.contains("(Number))"), "Got: {}", decompiled);

                    // Verify method call formatting with cast arguments
                    assert!(decompiled.contains("Boolean.valueOf((String) answer)"), "Got: {}", decompiled);

                    // Verify ternary expression recovery
                    assert!(decompiled.contains("return n.intValue() != 0 ? Boolean.TRUE : Boolean.FALSE;"), "Got: {}", decompiled);
                }
            }
        }
    }

    #[test]
    fn test_decompile_edge_cases_and_comparisons() {
        let temp_dir = std::env::temp_dir().join("jlsp_real_edge_cases_test");
        let _ = std::fs::create_dir_all(&temp_dir);
        let src_file = temp_dir.join("EdgeCases.java");
        let class_file = temp_dir.join("EdgeCases.class");

        let java_code = r#"public class EdgeCases {
    public static boolean testLong(long a, long b) {
        if (a > b) {
            return true;
        }
        return false;
    }

    public static int testPrecedence(int a, int b, int c) {
        return (a + b) * c;
    }

    public static boolean testBoolVars() {
        boolean active = true;
        boolean disabled = false;
        return active && !disabled;
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

                    // 1. Long comparison operator must not be inverted
                    assert!(decompiled.contains("if (a > b) {"), "Got: {}", decompiled);
                    assert!(!decompiled.contains("if (a <= b) {"), "Got: {}", decompiled);

                    // 2. Arithmetic precedence with parenthesis
                    assert!(decompiled.contains("(a + b) * c"), "Got: {}", decompiled);

                    // 3. Boolean variable declarations
                    assert!(decompiled.contains("boolean active = true;"), "Got: {}", decompiled);
                    assert!(decompiled.contains("boolean disabled = false;"), "Got: {}", decompiled);
                    assert!(!decompiled.contains("boolean active = 1;"), "Got: {}", decompiled);
                    assert!(!decompiled.contains("boolean disabled = 0;"), "Got: {}", decompiled);

                    // 4. Method declaration has space before opening brace
                    assert!(decompiled.contains("public static boolean testLong(long a, long b) {"), "Got: {}", decompiled);
                }
            }
        }
    }

    #[test]
    fn test_decompile_mybatis_result_context_class() {
        if std::path::Path::new("/tmp/ResultContext.class").exists() {
            let bytes = std::fs::read("/tmp/ResultContext.class").expect("read /tmp/ResultContext.class");
            let decompiled = decompile_class_bytes(&bytes).expect("decompile ResultContext.class");
            assert!(decompiled.contains("public interface ResultContext<T> {"));
            assert!(decompiled.contains("T getResultObject();"));
            assert!(decompiled.contains("int getResultCount();"));
            assert!(decompiled.contains("boolean isStopped();"));
            assert!(decompiled.contains("void stop();"));
            assert!(!decompiled.contains(":Ljava/lang/Object"));
        }
    }
}

