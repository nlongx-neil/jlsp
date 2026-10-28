use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum TypeSignature {
    Void,
    Boolean,
    Byte,
    Char,
    Short,
    Int,
    Long,
    Float,
    Double,
    Class(String), // Fully qualified name, e.g. "java.lang.String"
    Array(Box<TypeSignature>),
    TypeVariable(String), // e.g. "T"
    Wildcard,
    Unknown,
}

impl TypeSignature {
    pub fn simple_name(&self) -> String {
        match self {
            TypeSignature::Void => "void".to_string(),
            TypeSignature::Boolean => "boolean".to_string(),
            TypeSignature::Byte => "byte".to_string(),
            TypeSignature::Char => "char".to_string(),
            TypeSignature::Short => "short".to_string(),
            TypeSignature::Int => "int".to_string(),
            TypeSignature::Long => "long".to_string(),
            TypeSignature::Float => "float".to_string(),
            TypeSignature::Double => "double".to_string(),
            TypeSignature::Class(fqn) => {
                fqn.split('.').next_back().unwrap_or(fqn).to_string()
            }
            TypeSignature::Array(inner) => format!("{}[]", inner.simple_name()),
            TypeSignature::TypeVariable(name) => name.clone(),
            TypeSignature::Wildcard => "?".to_string(),
            TypeSignature::Unknown => "?".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodInfo {
    pub name: String,
    pub return_type: TypeSignature,
    pub parameters: Vec<(String, TypeSignature)>,
    pub is_static: bool,
    pub is_public: bool,
    pub is_constructor: bool,
    pub javadoc: Option<String>,
    #[serde(default)]
    pub body: Option<String>,
}

impl MethodInfo {
    #[inline]
    pub fn has_body(&self) -> bool {
        self.body.is_some()
    }

    /// Returns parameter name or fallback "arg{idx}" if name is empty
    pub fn parameter_name(&self, idx: usize) -> String {
        if let Some((name, _)) = self.parameters.get(idx) {
            if !name.is_empty() {
                return name.clone();
            }
        }
        format!("arg{}", idx)
    }

    /// Formats parameter display string e.g. "String name" or "int arg0"
    pub fn parameter_display_strings(&self) -> Vec<String> {
        self.parameters
            .iter()
            .enumerate()
            .map(|(idx, (name, typ))| {
                let p_name = if name.is_empty() {
                    format!("arg{}", idx)
                } else {
                    name.clone()
                };
                format!("{} {}", typ.simple_name(), p_name)
            })
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldInfo {
    pub name: String,
    pub field_type: TypeSignature,
    pub is_static: bool,
    pub is_public: bool,
    pub is_final: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClassKind {
    Class,
    Interface,
    Enum,
    Record,
    Annotation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClassInfo {
    pub fqn: String, // e.g., "java.util.List"
    pub simple_name: String,
    pub package_name: String,
    pub kind: ClassKind,
    pub super_class: Option<String>,
    pub interfaces: Vec<String>,
    pub methods: Vec<MethodInfo>,
    pub fields: Vec<FieldInfo>,
    pub source_path: Option<String>,
    pub is_public: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SymbolKind {
    Package,
    Class,
    Interface,
    Enum,
    Method,
    Field,
    Variable,
    Parameter,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SymbolDef {
    pub name: String,
    pub kind: SymbolKind,
    pub range: Range,
    pub selection_range: Range,
    pub detail: Option<String>,
    pub container_name: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_method_info_parameter_fallback() {
        let m = MethodInfo {
            name: "calculate".to_string(),
            return_type: TypeSignature::Int,
            parameters: vec![
                ("".to_string(), TypeSignature::Int),
                ("factor".to_string(), TypeSignature::Class("java.lang.String".to_string())),
                ("".to_string(), TypeSignature::Boolean),
            ],
            is_static: false,
            is_public: true,
            is_constructor: false,
            javadoc: None,
            body: None,
        };

        assert_eq!(m.parameter_name(0), "arg0");
        assert_eq!(m.parameter_name(1), "factor");
        assert_eq!(m.parameter_name(2), "arg2");

        let displays = m.parameter_display_strings();
        assert_eq!(displays, vec!["int arg0", "String factor", "boolean arg2"]);
    }
}
