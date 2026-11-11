//! FreeBASIC parser plugin - full-parse lightweight mode.
//!
//! FreeBASIC remains hidden for beta, but this crate no longer relies on
//! Python-hosted parsing. It recognizes the core structural nodes needed for
//! deterministic fallback behavior until a real grammar is vendored.

use intentumdiff_plugin_sdk::tree::{SemanticNode, SemanticNodeBuilder};

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
    let metadata = intentumdiff_plugin_sdk::metadata::parse_plugin_metadata(PLUGIN_METADATA);
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

/// The line without its trailing `'` / ` REM ` comment. Comment markers inside a
/// double-quoted string literal (FreeBASIC escapes `"` as `""`) are content, not comments.
fn strip_inline_comment(line: &str) -> &str {
    let bytes = line.as_bytes();
    let mut in_string = false;
    for (i, &byte) in bytes.iter().enumerate() {
        match byte {
            b'"' => in_string = !in_string,
            b'\'' if !in_string => return &line[..i],
            b' ' | b'\t'
                if !in_string
                    && bytes.len() >= i + 5
                    && bytes[i + 1..i + 4].eq_ignore_ascii_case(b"rem")
                    && matches!(bytes[i + 4], b' ' | b'\t') =>
            {
                return &line[..i];
            }
            _ => {}
        }
    }
    line
}

/// UTF-8 byte column of the first non-whitespace character (multibyte indentation such as
/// U+3000 counts its bytes, not one per character).
fn indent_col(line: &str) -> u32 {
    (line.len() - line.trim_start().len()) as u32
}

/// UTF-8 byte column just past the line's code: trailing comment, trailing whitespace and
/// a CRLF line's `\r` are excluded. Positions are over raw LF-separated rows, so for CRLF
/// input the `\r` is a line-terminator byte, never part of a node's extent.
fn code_end_col(line: &str) -> u32 {
    strip_inline_comment(line).trim_end().len() as u32
}

/// Zero-based (row, UTF-8 byte column) of the end of `source`.
fn source_end(source: &str) -> (u32, u32) {
    let row = source.bytes().filter(|&b| b == b'\n').count() as u32;
    let col = source.len() - source.rfind('\n').map_or(0, |i| i + 1);
    (row, col as u32)
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

/// Declaration text with leading modifiers removed, and whether `Declare` was among them.
fn normalized_decl(line: &str) -> (&str, bool) {
    let mut declared = false;
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
                declared |= modifier == "declare ";
                rest = rest[modifier.len()..].trim_start();
                changed = true;
                break;
            }
        }
        if !changed {
            return (rest, declared);
        }
    }
}

fn identifier_after(line: &str, keyword: &str) -> String {
    let (rest, _) = normalized_decl(line);
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
    let (rest, declared) = normalized_decl(line);
    // `Declare Function F(...) As T` is a body-less prototype (also how members are declared
    // inside a Type); opening a block for it swallowed every following line up to the next
    // `End Function`, so the block's extent and its parent's were wrong.
    if declared {
        return None;
    }
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
            let after = lower[keyword.len()..].trim_start();
            // `Function = value` assigns the function result inside its body; it is not a
            // nested declaration (that also swallowed the enclosing `End Function`).
            let boundary = keyword.ends_with(' ')
                || after.len() < lower.len() - keyword.len()
                || after.is_empty()
                || after.starts_with('(');
            if !boundary || after.starts_with('=') {
                return None;
            }
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
    if normalized_decl(line).1 {
        return Some("declare_statement");
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

/// Parse lines until `terminator` (or end of input). Returns the nodes and, when the
/// terminator line was found, its zero-based row.
fn parse_nodes(
    lines: &[&str],
    index: &mut usize,
    terminator: Option<&str>,
    parent_class: Option<&str>,
    id_prefix: &str,
) -> (Vec<RawNode>, Option<usize>) {
    let mut nodes = Vec::new();
    while *index < lines.len() {
        let line = lines[*index];
        let lower = lower_trimmed(line);
        if let Some(end) = terminator {
            if lower.starts_with(end) {
                let row = *index;
                *index += 1;
                return (nodes, Some(row));
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
            let (children, closing_row) = parse_nodes(
                lines,
                index,
                Some(end),
                child_parent,
                &format!("{}.{}", id_prefix, line_index),
            );
            // A block ends where its closing `End ...` line's code ends (issue #4: the end
            // row came from the last child but the end column from the opening header, which
            // overran short closing lines). An unterminated block ends with its last child,
            // or its own header when it has none.
            let (end_line, end_col) = match closing_row {
                Some(row) => (row as u32, code_end_col(lines[row])),
                None => children
                    .last()
                    .map(|child| (child.end_line, child.end_col))
                    .unwrap_or((line_index as u32, code_end_col(line))),
            };
            let mut node = RawNode {
                id: format!("{}.{}", id_prefix, line_index),
                node_type: node_type.to_string(),
                label,
                start_line: line_index as u32,
                start_col: indent_col(line),
                end_line,
                end_col,
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
                end_col: code_end_col(line),
                children: Vec::new(),
                parent_type: None,
            });
        }
        *index += 1;
    }
    (nodes, None)
}

fn process_impl(source: &str) -> String {
    let lines: Vec<&str> = source.lines().collect();
    let mut index = 0;
    let (children, _) = parse_nodes(&lines, &mut index, None, None, "0");
    // The root spans the whole source, including a final LF (whose end is row+1, col 0).
    let (end_line, end_col) = source_end(source);
    let root = RawNode {
        id: "0".to_string(),
        node_type: "source_file".to_string(),
        label: "source_file".to_string(),
        start_line: 0,
        start_col: 0,
        end_line,
        end_col,
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
    use intentumdiff_plugin_sdk::testing as t;

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

    // ---- Source positions (issue #4) ----

    fn tree(source: &str) -> SemanticNode {
        serde_json::from_str(&process_impl(source)).expect("semantic JSON")
    }

    fn span(node: &SemanticNode) -> (u32, u32, u32, u32) {
        let p = &node.position;
        (p.start_line, p.start_col, p.end_line, p.end_col)
    }

    /// Every endpoint is ordered, inside the raw LF-separated row, on a UTF-8 boundary;
    /// parents contain their children and ids are globally unique.
    fn assert_valid_positions(source: &str) -> SemanticNode {
        let rows: Vec<&str> = source.split('\n').collect();
        let root = tree(source);
        let mut ids = std::collections::HashSet::new();
        fn walk<'a>(
            node: &'a SemanticNode,
            rows: &[&str],
            ids: &mut std::collections::HashSet<&'a str>,
        ) {
            let (sl, sc, el, ec) = span(node);
            assert!((sl, sc) <= (el, ec), "unordered {:?} {node:?}", span(node));
            for (line, col) in [(sl, sc), (el, ec)] {
                let row = rows.get(line as usize).unwrap_or_else(|| panic!("row {line} {node:?}"));
                assert!(col as usize <= row.len(), "column out of bounds {:?}", span(node));
                assert!(row.is_char_boundary(col as usize), "not a UTF-8 boundary");
            }
            assert!(ids.insert(node.id.as_str()), "duplicate id {}", node.id);
            for child in &node.children {
                let (csl, csc, cel, cec) = span(child);
                assert!((sl, sc) <= (csl, csc) && (cel, cec) <= (el, ec), "{} escapes {}", child.id, node.id);
                walk(child, rows, ids);
            }
        }
        walk(&root, &rows, &mut ids);
        assert_eq!(process_impl(source), process_impl(source), "deterministic");
        root
    }

    #[test]
    fn function_ends_at_its_closing_line_not_its_header_width() {
        // The issue's exact input: the end column was 35 (the header's length) on row 2,
        // whose `End Function` is only 12 bytes long.
        let source = "Function F(x As Integer) As Integer\n  Return x\nEnd Function";
        let root = assert_valid_positions(source);
        let function = &root.children[0];
        assert_eq!(function.id, "0.0");
        assert_eq!(function.node_type, "function_declaration");
        assert_eq!(span(function), (0, 0, 2, 12));
        assert_eq!(span(&function.children[0]), (1, 2, 1, 10));
        assert_eq!(span(&root), (0, 0, 2, 12));
    }

    #[test]
    fn final_newline_and_crlf_conventions() {
        let lf = assert_valid_positions("Sub S()\n  Print 1\nEnd Sub\n");
        assert_eq!(span(&lf.children[0]), (0, 0, 2, 7));
        assert_eq!(span(&lf), (0, 0, 3, 0), "root covers the final LF");
        // CRLF: `\r` is a line-terminator byte, never inside a node's extent.
        let crlf = assert_valid_positions("Sub S()\r\n  Print 1\r\nEnd Sub\r\n");
        assert_eq!(span(&crlf.children[0]), (0, 0, 2, 7));
        assert_eq!(span(&crlf.children[0].children[0]), (1, 2, 1, 9));
        assert_eq!(span(&crlf), (0, 0, 3, 0));
    }

    #[test]
    fn empty_and_blank_input() {
        assert_eq!(span(&assert_valid_positions("")), (0, 0, 0, 0));
        assert_eq!(span(&assert_valid_positions("   \n  ")), (0, 0, 1, 2));
    }

    #[test]
    fn multibyte_content_and_indentation_use_byte_columns() {
        // U+3000 IDEOGRAPHIC SPACE is 3 bytes of indentation; é and 🦀 are 2 and 4 bytes.
        let source = "Sub Grüße()\n\u{3000}Print \"é🦀\" ' 注释\nEnd Sub\n";
        let root = assert_valid_positions(source);
        let print = &root.children[0].children[0];
        assert_eq!(span(print), (1, 3, 1, 3 + "Print \"é🦀\"".len() as u32));
        assert_eq!(print.label, "Print \"é🦀\"");
    }

    #[test]
    fn adjacent_and_nested_blocks_keep_their_own_extents() {
        let source = "Type Point\n  x As Integer\n  Declare Function Len() As Integer\nEnd Type\n\
                      Function Point.Len() As Integer\n  Function = x\nEnd Function\n\
                      Sub A()\nEnd Sub\nSub B() ' trailing\n  Print 1\nEnd Sub   ' done\n";
        let root = assert_valid_positions(source);
        let kinds: Vec<(&str, &str, (u32, u32, u32, u32))> = root
            .children
            .iter()
            .map(|c| (c.node_type.as_str(), c.label.as_str(), span(c)))
            .collect();
        assert_eq!(
            kinds,
            vec![
                ("type_declaration", "Point", (0, 0, 3, 8)),
                ("function_declaration", "Point.Len", (4, 0, 6, 12)),
                ("sub_declaration", "A", (7, 0, 8, 7)),
                ("sub_declaration", "B", (9, 0, 11, 7)),
            ]
        );
        // A `Declare` prototype is a one-line member, not a block swallowing `End Type`.
        let member = &root.children[0].children[1];
        assert_eq!((member.node_type.as_str(), span(member)), ("declare_statement", (2, 2, 2, 35)));
        // `Function = x` assigns the result; it does not open a nested function.
        let body = &root.children[1].children;
        assert_eq!(body.len(), 1);
        assert_eq!(body[0].node_type, "assignment_statement");
    }

    #[test]
    fn unterminated_block_ends_with_its_last_child() {
        let root = assert_valid_positions("Sub S()\n  Print 1\n\n");
        assert_eq!(span(&root.children[0]), (0, 0, 1, 9));
        let header_only = assert_valid_positions("Function F() As Integer   ");
        assert_eq!(span(&header_only.children[0]), (0, 0, 0, 23));
    }

    #[test]
    fn comment_markers_inside_strings_are_content() {
        let root = assert_valid_positions("Print \"it's a rem test\" ' real comment\n");
        assert_eq!(root.children[0].label, "Print \"it's a rem test\"");
        assert_eq!(span(&root.children[0]), (0, 0, 0, 23));
    }
}
