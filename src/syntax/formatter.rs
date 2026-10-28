/// Java source code formatter with Eclipse XML profile support
#[derive(Debug, Clone)]
pub struct JavaFormatter {
    pub tab_size: usize,
    pub insert_spaces: bool,
    pub brace_position_next_line: bool,
    pub insert_space_after_comma: bool,
    pub max_consecutive_blank_lines: usize,
}

impl Default for JavaFormatter {
    fn default() -> Self {
        Self {
            tab_size: 4,
            insert_spaces: true,
            brace_position_next_line: false,
            insert_space_after_comma: true,
            max_consecutive_blank_lines: 2,
        }
    }
}

impl JavaFormatter {
    pub fn new(tab_size: usize, insert_spaces: bool) -> Self {
        Self {
            tab_size,
            insert_spaces,
            brace_position_next_line: false,
            insert_space_after_comma: true,
            max_consecutive_blank_lines: 2,
        }
    }

    /// Load formatting options from Eclipse format XML configuration
    pub fn load_from_eclipse_xml(xml_content: &str) -> Self {
        let mut tab_size = 4;
        let mut insert_spaces = true;
        let mut brace_position_next_line = false;
        let mut insert_space_after_comma = true;
        let mut max_consecutive_blank_lines = 2;

        for line in xml_content.lines() {
            if line.contains("org.eclipse.jdt.core.formatter.tabulation.size") {
                if let Some(val_idx) = line.find("value=\"") {
                    let rest = &line[val_idx + 7..];
                    if let Some(end_idx) = rest.find('"') {
                        if let Ok(num) = rest[..end_idx].parse::<usize>() {
                            tab_size = num;
                        }
                    }
                }
            } else if line.contains("org.eclipse.jdt.core.formatter.tabulation.char") {
                if line.contains("value=\"space\"") {
                    insert_spaces = true;
                } else if line.contains("value=\"tab\"") {
                    insert_spaces = false;
                }
            } else if line.contains("org.eclipse.jdt.core.formatter.brace_position_for_type_declaration")
                || line.contains("org.eclipse.jdt.core.formatter.brace_position_for_method_declaration")
                || line.contains("org.eclipse.jdt.core.formatter.brace_position_for_block")
            {
                if line.contains("value=\"next_line\"") {
                    brace_position_next_line = true;
                } else if line.contains("value=\"end_of_line\"") || line.contains("value=\"same_line\"") {
                    brace_position_next_line = false;
                }
            } else if line.contains("org.eclipse.jdt.core.formatter.insert_space_after_comma_in_method_invocation_arguments")
                || line.contains("org.eclipse.jdt.core.formatter.insert_space_after_comma_in_method_declaration_parameters")
            {
                if line.contains("value=\"insert\"") || line.contains("value=\"true\"") {
                    insert_space_after_comma = true;
                } else if line.contains("value=\"do not insert\"") || line.contains("value=\"false\"") {
                    insert_space_after_comma = false;
                }
            } else if line.contains("org.eclipse.jdt.core.formatter.number_of_empty_lines_to_preserve") {
                if let Some(val_idx) = line.find("value=\"") {
                    let rest = &line[val_idx + 7..];
                    if let Some(end_idx) = rest.find('"') {
                        if let Ok(num) = rest[..end_idx].parse::<usize>() {
                            max_consecutive_blank_lines = num;
                        }
                    }
                }
            }
        }

        Self {
            tab_size,
            insert_spaces,
            brace_position_next_line,
            insert_space_after_comma,
            max_consecutive_blank_lines,
        }
    }

    pub fn format(&self, source: &str) -> String {
        let mut result = String::with_capacity(source.len());
        let mut indent_level: usize = 0;
        let indent_unit = if self.insert_spaces {
            " ".repeat(self.tab_size)
        } else {
            "\t".to_string()
        };

        let mut in_block_comment = false;
        let mut in_text_block = false;
        let mut blank_line_count = 0;

        for raw_line in source.lines() {
            let line = raw_line.trim();

            if line.is_empty() {
                blank_line_count += 1;
                if blank_line_count <= self.max_consecutive_blank_lines {
                    result.push('\n');
                }
                continue;
            }
            blank_line_count = 0;

            // Count leading closing braces that reduce this line's indent
            let mut leading_close = 0;
            if !in_block_comment && !in_text_block {
                for ch in line.chars() {
                    if ch == '}' {
                        leading_close += 1;
                    } else if !ch.is_whitespace() {
                        break;
                    }
                }
            }

            let effective_indent = indent_level.saturating_sub(leading_close);
            if in_block_comment && line.starts_with('*') && !line.starts_with("*/") {
                for _ in 0..effective_indent {
                    result.push_str(&indent_unit);
                }
                result.push(' ');
                result.push_str(line);
                result.push('\n');
            } else {
                for _ in 0..effective_indent {
                    result.push_str(&indent_unit);
                }
                result.push_str(line);
                result.push('\n');
            }

            // Lexical scan of the line to compute net brace change
            let chars: Vec<char> = line.chars().collect();
            let mut i = 0;
            let mut opens = 0;
            let mut closes = 0;

            while i < chars.len() {
                if in_block_comment {
                    if chars[i] == '*' && chars.get(i + 1) == Some(&'/') {
                        in_block_comment = false;
                        i += 2;
                    } else {
                        i += 1;
                    }
                    continue;
                }

                if in_text_block {
                    if chars[i] == '"' && chars.get(i + 1) == Some(&'"') && chars.get(i + 2) == Some(&'"') {
                        in_text_block = false;
                        i += 3;
                    } else {
                        i += 1;
                    }
                    continue;
                }

                // Check text block start: """
                if chars[i] == '"' && chars.get(i + 1) == Some(&'"') && chars.get(i + 2) == Some(&'"') {
                    in_text_block = true;
                    i += 3;
                    continue;
                }

                // Check single line comment: //
                if chars[i] == '/' && chars.get(i + 1) == Some(&'/') {
                    break;
                }

                // Check block comment start: /*
                if chars[i] == '/' && chars.get(i + 1) == Some(&'*') {
                    in_block_comment = true;
                    i += 2;
                    continue;
                }

                // Check string literal: "..."
                if chars[i] == '"' {
                    i += 1;
                    while i < chars.len() {
                        if chars[i] == '\\' {
                            i += 2;
                        } else if chars[i] == '"' {
                            i += 1;
                            break;
                        } else {
                            i += 1;
                        }
                    }
                    continue;
                }

                // Check character literal: '...'
                if chars[i] == '\'' {
                    i += 1;
                    while i < chars.len() {
                        if chars[i] == '\\' {
                            i += 2;
                        } else if chars[i] == '\'' {
                            i += 1;
                            break;
                        } else {
                            i += 1;
                        }
                    }
                    continue;
                }

                if chars[i] == '{' {
                    opens += 1;
                } else if chars[i] == '}' {
                    closes += 1;
                }
                i += 1;
            }

            if opens > closes {
                indent_level += opens - closes;
            } else if closes > opens {
                indent_level = indent_level.saturating_sub(closes - opens);
            }
        }

        result
    }

    /// Format specific line range [start_line, end_line]
    pub fn format_range(&self, source: &str, start_line: u32, end_line: u32) -> String {
        let full = self.format(source);
        let full_lines: Vec<&str> = full.lines().collect();
        let s = start_line as usize;
        let e = (end_line as usize).min(full_lines.len().saturating_sub(1));
        if s <= e && s < full_lines.len() {
            full_lines[s..=e].join("\n") + "\n"
        } else {
            full
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_java_code() {
        let unformatted = "public class Foo {\npublic void bar() {\nSystem.out.println(1);\n}\n}";
        let formatter = JavaFormatter::new(4, true);
        let formatted = formatter.format(unformatted);
        let expected = "public class Foo {\n    public void bar() {\n        System.out.println(1);\n    }\n}\n";
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_with_char_literals_comments_and_text_blocks() {
        let unformatted = r#"public class EdgeCases {
public void test() {
char c1 = '}';
char c2 = '{';
// single comment {
/*
block comment {
*/
String text = """
SELECT {id} FROM table
""";
int x = 42;
}
}"#;
        let formatter = JavaFormatter::new(4, true);
        let formatted = formatter.format(unformatted);
        let expected = r#"public class EdgeCases {
    public void test() {
        char c1 = '}';
        char c2 = '{';
        // single comment {
        /*
        block comment {
        */
        String text = """
        SELECT {id} FROM table
        """;
        int x = 42;
    }
}
"#;
        assert_eq!(formatted, expected);
    }

    #[test]
    fn test_format_eclipse_xml_extended_options() {
        let xml = r#"
            <setting id="org.eclipse.jdt.core.formatter.tabulation.size" value="2"/>
            <setting id="org.eclipse.jdt.core.formatter.tabulation.char" value="space"/>
            <setting id="org.eclipse.jdt.core.formatter.brace_position_for_type_declaration" value="next_line"/>
            <setting id="org.eclipse.jdt.core.formatter.number_of_empty_lines_to_preserve" value="1"/>
        "#;
        let formatter = JavaFormatter::load_from_eclipse_xml(xml);
        assert_eq!(formatter.tab_size, 2);
        assert!(formatter.insert_spaces);
        assert!(formatter.brace_position_next_line);
        assert_eq!(formatter.max_consecutive_blank_lines, 1);

        let unformatted = "public class A {\n\n\n\n  int x = 1;\n}";
        let formatted = formatter.format(unformatted);
        assert!(!formatted.contains("\n\n\n"));
    }
}
