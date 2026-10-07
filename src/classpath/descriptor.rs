use crate::model::TypeSignature;

/// Parse JVM bytecode field descriptor, e.g. "Ljava/lang/String;" or "[I"
pub fn parse_field_descriptor(desc: &str) -> (TypeSignature, &str) {
    let mut chars = desc.chars();
    match chars.next() {
        Some('V') => (TypeSignature::Void, &desc[1..]),
        Some('Z') => (TypeSignature::Boolean, &desc[1..]),
        Some('B') => (TypeSignature::Byte, &desc[1..]),
        Some('C') => (TypeSignature::Char, &desc[1..]),
        Some('S') => (TypeSignature::Short, &desc[1..]),
        Some('I') => (TypeSignature::Int, &desc[1..]),
        Some('J') => (TypeSignature::Long, &desc[1..]),
        Some('F') => (TypeSignature::Float, &desc[1..]),
        Some('D') => (TypeSignature::Double, &desc[1..]),
        Some('[') => {
            let (elem_ty, rest) = parse_field_descriptor(&desc[1..]);
            (TypeSignature::Array(Box::new(elem_ty)), rest)
        }
        Some('L') => {
            if let Some(semi_pos) = desc.find(';') {
                let class_internal = &desc[1..semi_pos];
                let fqn = class_internal.replace('/', ".");
                (TypeSignature::Class(fqn), &desc[semi_pos + 1..])
            } else {
                (TypeSignature::Unknown, "")
            }
        }
        Some('T') => {
            // Type variable in generic signature: TT;
            if let Some(semi_pos) = desc.find(';') {
                let var_name = &desc[1..semi_pos];
                (TypeSignature::TypeVariable(var_name.to_string()), &desc[semi_pos + 1..])
            } else {
                (TypeSignature::Unknown, "")
            }
        }
        _ => (TypeSignature::Unknown, ""),
    }
}

/// Parse JVM bytecode method descriptor, e.g. "(Ljava/lang/String;I)V"
pub fn parse_method_descriptor(desc: &str) -> (Vec<TypeSignature>, TypeSignature) {
    if !desc.starts_with('(') {
        return (Vec::new(), TypeSignature::Unknown);
    }

    let mut params = Vec::new();
    let mut rem = &desc[1..];

    while !rem.is_empty() && !rem.starts_with(')') {
        let (param_ty, next_rem) = parse_field_descriptor(rem);
        params.push(param_ty);
        rem = next_rem;
    }

    if rem.starts_with(')') {
        rem = &rem[1..];
    }

    let (ret_ty, _) = parse_field_descriptor(rem);
    (params, ret_ty)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_field_descriptor() {
        let (ty, rem) = parse_field_descriptor("Ljava/lang/String;");
        assert_eq!(ty, TypeSignature::Class("java.lang.String".into()));
        assert_eq!(rem, "");

        let (ty, rem) = parse_field_descriptor("[I");
        assert_eq!(ty, TypeSignature::Array(Box::new(TypeSignature::Int)));
        assert_eq!(rem, "");
    }

    #[test]
    fn test_method_descriptor() {
        let (params, ret) = parse_method_descriptor("(Ljava/lang/String;I)V");
        assert_eq!(params.len(), 2);
        assert_eq!(params[0], TypeSignature::Class("java.lang.String".into()));
        assert_eq!(params[1], TypeSignature::Int);
        assert_eq!(ret, TypeSignature::Void);
    }
}
