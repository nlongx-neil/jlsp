/// Java source code formatter
pub struct JavaFormatter {
    pub tab_size: usize,
    pub insert_spaces: bool,
}

impl Default for JavaFormatter {
    fn default() -> Self {
        Self {
            tab_size: 4,
            insert_spaces: true,
        }
    }
}

impl JavaFormatter {
    pub fn new(tab_size: usize, insert_spaces: bool) -> Self {
        Self {
            tab_size,
            insert_spaces,
        }
    }

    /// Load formatting options from Eclipse format XML configuration
    pub fn load_from_eclipse_xml(xml_content: &str) -> Self {
        let mut tab_size = 4;
        let mut insert_spaces = true;

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
            }
        }

        Self {
            tab_size,
            insert_spaces,
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

        for raw_line in source.lines() {
            let line = raw_line.trim();

            if line.is_empty() {
                result.push('\n');
                continue;
            }

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
}
