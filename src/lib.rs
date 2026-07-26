//! FreeBASIC parser plugin - full-parse lightweight mode.
//!
//! FreeBASIC remains hidden for beta, but this crate no longer relies on
//! Python-hosted parsing. It recognizes the core structural nodes needed for
//! deterministic fallback behavior until a real grammar is vendored.

use intentdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

wit_bindgen::generate!({
    path: "wit/plugin.wit",
    world: "parser-plugin",
});

use crate::exports::intentdiff::plugin::parser::ExamplePair;
use crate::exports::intentdiff::plugin::parser::Guest;
use crate::exports::intentdiff::plugin::parser::LanguageInfoRecord;
use crate::exports::intentdiff::plugin::parser::ParserMode;

const PLUGIN_METADATA: &str = include_str!("../plugin_metadata.info");

fn language_info_for(ids: Vec<String>) -> Vec<LanguageInfoRecord> {
    let metadata = intentdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
    ids.into_iter()
        .map(|language_id| {
            let info = metadata.language_or_default(&language_id);
            LanguageInfoRecord {
                language_id: info.language_id,
                language_name: info.language_name,
                language_short_name: info.language_short_name,
                monaco_language: info.monaco_language,
                default_filename: info.default_filename,
                language_file_extensions: info.language_file_extensions,
                author: metadata.author().to_string(),
                plugin_version: metadata.plugin_version().to_string(),
                last_updated: metadata.last_updated().to_string(),
            }
        })
        .collect()
}

struct FreeBASICParser;

const TRIVIA: &[&str] = &["comment", "line_comment", "block_comment", "rem_comment"];

#[derive(Debug, Clone)]
struct RawNode {
    id: String,
    node_type: String,
    label: String,
    start_line: u32,
    start_col: u32,
    end_line: u32,
    end_col: u32,
    children: Vec<RawNode>,
    parent_type: Option<String>,
}

impl RawNode {
    fn semantic(self) -> SemanticNode {
        let mut builder = SemanticNodeBuilder::new(
            self.id,
            self.node_type,
            self.label,
            self.start_line,
            self.start_col,
            self.end_line,
            self.end_col,
            "",
        )
        .children(self.children.into_iter().map(RawNode::semantic).collect());
        if let Some(parent_type) = self.parent_type {
            builder = builder.parent_type(parent_type);
        }
        builder.build()
    }
}

fn strip_inline_comment(line: &str) -> &str {
    let apostrophe = line.find('\'');
    let rem = line.to_ascii_lowercase().find(" rem ");
    match (apostrophe, rem) {
        (Some(a), Some(r)) => &line[..a.min(r)],
        (Some(a), None) => &line[..a],
        (None, Some(r)) => &line[..r],
        (None, None) => line,
    }
}

fn indent_col(line: &str) -> u32 {
    line.chars().take_while(|ch| ch.is_whitespace()).count() as u32
}

fn lower_trimmed(line: &str) -> String {
    strip_inline_comment(line).trim().to_ascii_lowercase()
}

fn is_class_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "type_declaration"
            | "class_declaration"
            | "union_declaration"
            | "enum_declaration"
            | "namespace_declaration"
            | "scope_block"
    )
}

fn is_method_like(node_type: &str) -> bool {
    matches!(
        node_type,
        "sub_declaration"
            | "function_declaration"
            | "property_declaration"
            | "constructor_declaration"
            | "destructor_declaration"
    )
}

fn normalized_decl(line: &str) -> &str {
    let mut rest = strip_inline_comment(line).trim();
    loop {
        let lower = rest.to_ascii_lowercase();
        let mut changed = false;
        for modifier in [
            "public ",
            "private ",
            "protected ",
            "static ",
            "abstract ",
            "virtual ",
            "override ",
            "declare ",
            "byref ",
            "byval ",
        ] {
            if lower.starts_with(modifier) {
                rest = rest[modifier.len()..].trim_start();
                changed = true;
                break;
            }
        }
        if !changed {
            return rest;
        }
    }
}

fn identifier_after(line: &str, keyword: &str) -> String {
    let rest = normalized_decl(line);
    let lower = rest.to_ascii_lowercase();
    if !lower.starts_with(keyword) {
        return rest.to_string();
    }
    let label: String = rest[keyword.len()..]
        .trim_start()
        .chars()
        .take_while(|ch| ch.is_alphanumeric() || *ch == '_' || *ch == '.')
        .collect();
    if label.is_empty() {
        "(anonymous)".to_string()
    } else {
        label
    }
}

fn declaration_kind(line: &str) -> Option<(&'static str, &'static str, &'static str)> {
    let rest = normalized_decl(line);
    let lower = rest.to_ascii_lowercase();
    for (keyword, node_type, terminator) in [
        ("namespace ", "namespace_declaration", "end namespace"),
        ("type ", "type_declaration", "end type"),
        ("class ", "class_declaration", "end class"),
        ("union ", "union_declaration", "end union"),
        ("enum ", "enum_declaration", "end enum"),
        ("scope ", "scope_block", "end scope"),
        ("sub ", "sub_declaration", "end sub"),
        ("function ", "function_declaration", "end function"),
        ("property ", "property_declaration", "end property"),
        ("constructor", "constructor_declaration", "end constructor"),
        ("destructor", "destructor_declaration", "end destructor"),
    ] {
        if lower.starts_with(keyword) {
            return Some((keyword, node_type, terminator));
        }
    }
    None
}

fn statement_kind(line: &str) -> Option<&'static str> {
    let lower = lower_trimmed(line);
    if lower.is_empty() || lower.starts_with('\'') || lower.starts_with("rem ") {
        return None;
    }
    if lower.starts_with("#include") || lower.starts_with("#define") {
        return Some("preprocessor_directive");
    }
    if lower.starts_with("return ") {
        return Some("return_statement");
    }
    if lower.starts_with("print ") {
        return Some("print_statement");
    }
    if lower.contains('=') {
        return Some("assignment_statement");
    }
    if lower.contains('(') && lower.contains(')') {
        return Some("invocation_expression");
    }
    Some("expression_statement")
}

fn parse_nodes(
    lines: &[&str],
    index: &mut usize,
    terminator: Option<&str>,
    parent_class: Option<&str>,
    id_prefix: &str,
) -> Vec<RawNode> {
    let mut nodes = Vec::new();
    while *index < lines.len() {
        let line = lines[*index];
        let lower = lower_trimmed(line);
        if let Some(end) = terminator {
            if lower.starts_with(end) {
                *index += 1;
                break;
            }
        }
        if lower.is_empty() || lower.starts_with('\'') || lower.starts_with("rem ") {
            *index += 1;
            continue;
        }

        let line_index = *index;
        if let Some((keyword, node_type, end)) = declaration_kind(line) {
            *index += 1;
            let label = identifier_after(line, keyword);
            let child_parent = if is_class_like(node_type) {
                Some(label.as_str())
            } else {
                parent_class
            };
            let children = parse_nodes(
                lines,
                index,
                Some(end),
                child_parent,
                &format!("{}.{}", id_prefix, line_index),
            );
            let end_line = children
                .last()
                .map(|child| child.end_line)
                .unwrap_or(line_index as u32);
            let mut node = RawNode {
                id: format!("{}.{}", id_prefix, line_index),
                node_type: node_type.to_string(),
                label,
                start_line: line_index as u32,
                start_col: indent_col(line),
                end_line,
                end_col: line.len() as u32,
                children,
                parent_type: None,
            };
            if is_method_like(node_type) {
                node.parent_type = parent_class.map(str::to_string);
            }
            nodes.push(node);
            continue;
        }

        if let Some(node_type) = statement_kind(line) {
            nodes.push(RawNode {
                id: format!("{}.{}", id_prefix, line_index),
                node_type: node_type.to_string(),
                label: strip_inline_comment(line).trim().to_string(),
                start_line: line_index as u32,
                start_col: indent_col(line),
                end_line: line_index as u32,
                end_col: line.len() as u32,
                children: Vec::new(),
                parent_type: None,
            });
        }
        *index += 1;
    }
    nodes
}

fn process_impl(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut index = 0;
    let children = parse_nodes(&lines, &mut index, None, None, "0");
    let root = RawNode {
        id: "0".to_string(),
        node_type: "source_file".to_string(),
        label: "source_file".to_string(),
        start_line: 0,
        start_col: 0,
        end_line: lines.len().saturating_sub(1) as u32,
        end_col: lines.last().map(|line| line.len()).unwrap_or(0) as u32,
        children,
        parent_type: None,
    };
    match serde_json::to_string(&root.semantic()) {
        Ok(s) => s,
        Err(e) => format!(r#"{{"error":"Serialisation error: {}"}}"#, e),
    }
}

impl Guest for FreeBASICParser {
    fn get_parser_mode() -> ParserMode {
        ParserMode::FullParse
    }

    fn grammar_id() -> String {
        "freebasic".to_string()
    }

    fn detect_language(filename: String, _content: String) -> String {
        let lower = filename.to_lowercase();
        if lower.ends_with(".bas") || lower.ends_with(".bi") {
            return "freebasic".to_string();
        }
        String::new()
    }

    fn preprocess_source(source: String) -> String {
        source
    }

    fn example(_language: String) -> ExamplePair {
        ExamplePair {
            old: "Sub Greet(name As String)\n    Print \"Hello, \" & name\nEnd Sub\n\nFunction Add(a As Integer, b As Integer) As Integer\n    Return a + b\nEnd Function\n".to_string(),
            new: "Sub Greet(name As String)\n    Print \"Hello, \" & name & \"!\"\nEnd Sub\n\nFunction Add(ByVal a As Integer, ByVal b As Integer) As Integer\n    Return a + b\nEnd Function\n\nFunction Multiply(ByVal a As Integer, ByVal b As Integer) As Integer\n    Return a * b\nEnd Function\n".to_string(),
        }
    }

    fn process(input: String, _language: String, _filename: String) -> String {
        process_impl(&input)
    }

    fn trivia_node_types() -> Vec<String> {
        TRIVIA.iter().map(|s| s.to_string()).collect()
    }

    fn language_ids() -> Vec<String> {
        vec!["freebasic".to_string()]
    }

    fn language_info() -> Vec<LanguageInfoRecord> {
        language_info_for(Self::language_ids())
    }

    fn priority() -> i32 {
        0
    }
}

export!(FreeBASICParser);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exports::intentdiff::plugin::parser::Guest;
    use intentdiff_plugin_sdk::testing as t;

    #[test]
    fn grammar_id_nonempty() {
        assert!(!FreeBASICParser::grammar_id().is_empty());
    }

    #[test]
    fn language_ids_contain_grammar_id() {
        let gid = FreeBASICParser::grammar_id();
        let ids = FreeBASICParser::language_ids();
        assert!(ids.contains(&gid));
    }

    #[test]
    fn parser_mode_is_full_parse() {
        assert!(matches!(
            FreeBASICParser::get_parser_mode(),
            ParserMode::FullParse
        ));
    }

    #[test]
    fn detect_language_known_ext() {
        let r = FreeBASICParser::detect_language("test.bas".to_string(), "".to_string());
        assert_eq!(r.as_str(), "freebasic");
    }

    #[test]
    fn detect_language_unknown_ext() {
        let r = FreeBASICParser::detect_language(
            "test.xyz_notareal_ext_9z8y".to_string(),
            "".to_string(),
        );
        assert_eq!(r.as_str(), "");
    }

    #[test]
    fn process_impl_accepts_raw_example_source() {
        let example = FreeBASICParser::example(FreeBASICParser::grammar_id());
        let out = process_impl(&example.old);
        t::assert_valid_json(&out, "process(raw example)");
        assert!(out.contains("sub_declaration"));
        assert!(out.contains("function_declaration"));
    }

    #[test]
    fn process_impl_recognizes_preprocessor_directives() {
        let out = process_impl("#include \"foo.bi\"\n#define ANSWER 42\n");
        t::assert_valid_json(&out, "process(preprocessor)");
        assert!(out.contains("preprocessor_directive"));
    }

    #[test]
    fn process_impl_empty_returns_valid_json() {
        let out = process_impl("");
        t::assert_valid_json(&out, "process(empty)");
    }

    #[test]
    fn process_impl_whitespace_returns_valid_json() {
        let out = process_impl("   \n  ");
        t::assert_valid_json(&out, "process(whitespace)");
    }
}
