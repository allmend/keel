//! Canonical HCL formatting, as the HCL reference implementation's
//! formatter (hclwrite) does it: only whitespace changes. Each line is
//! indented two spaces per open bracket, tokens are spaced by a fixed set of
//! rules, and the `=` of consecutive single-line attributes and trailing
//! comments are aligned.
//!
//! Also the commands built on it: `keel config fmt` and
//! `keel config validate`.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Newline,
    Comment,
    Ident,
    Number,
    OQuote,
    CQuote,
    QuotedLit,
    OHeredoc,
    CHeredoc,
    StringLit,
    TemplateInterp,
    TemplateControl,
    TemplateSeqEnd,
    OBrace,
    CBrace,
    OBrack,
    CBrack,
    OParen,
    CParen,
    Equal,
    Comma,
    Dot,
    Ellipsis,
    Colon,
    Question,
    Minus,
    Bang,
    Operator,
    Other,
}

#[derive(Debug, Clone)]
struct Token {
    kind: Kind,
    text: String,
    spaces: usize,
}

/// Where the lexer is: plain expression (with the depth of `{` opened inside
/// a template interpolation), a quoted string, or a heredoc body.
enum Mode {
    Expr { braces: usize, in_template: bool },
    Quoted,
    Heredoc { marker: String },
}

fn lex(src: &str) -> Vec<Token> {
    let chars: Vec<char> = src.chars().collect();
    let mut tokens = Vec::new();
    let mut modes = vec![Mode::Expr { braces: 0, in_template: false }];
    let mut i = 0;
    let mut spaces = 0;
    let at = |i: usize, s: &str| s.chars().enumerate().all(|(k, c)| chars.get(i + k) == Some(&c));
    while i < chars.len() {
        match modes.last_mut().expect("a mode") {
            Mode::Quoted => {
                if chars[i] == '"' {
                    push(&mut tokens, Kind::CQuote, "\"".into(), &mut spaces);
                    modes.pop();
                    i += 1;
                } else if let Some((kind, len)) = template_open(&chars, i) {
                    push(&mut tokens, kind, chars[i..i + len].iter().collect(), &mut spaces);
                    modes.push(Mode::Expr { braces: 0, in_template: true });
                    i += len;
                } else {
                    let start = i;
                    while i < chars.len() && chars[i] != '"' && chars[i] != '\n' && template_open(&chars, i).is_none() {
                        if chars[i] == '\\' {
                            i += 1;
                        }
                        // `$${` and `%%{` are escapes, not templates.
                        if at(i, "$${") || at(i, "%%{") {
                            i += 2;
                        }
                        i += 1;
                    }
                    if i == start {
                        // An unterminated string: leave the rest of the line to the expression lexer.
                        modes.pop();
                        continue;
                    }
                    push(&mut tokens, Kind::QuotedLit, chars[start..i.min(chars.len())].iter().collect(), &mut spaces);
                }
            }
            Mode::Heredoc { marker } => {
                // A line that is the marker (optionally indented) closes it.
                let line_end = chars[i..].iter().position(|&c| c == '\n').map_or(chars.len(), |p| i + p);
                let line: String = chars[i..line_end].iter().collect();
                if line.trim_start() == marker.as_str() {
                    push(&mut tokens, Kind::CHeredoc, line, &mut spaces);
                    modes.pop();
                    i = line_end;
                } else if let Some((kind, len)) = template_open(&chars, i) {
                    push(&mut tokens, kind, chars[i..i + len].iter().collect(), &mut spaces);
                    modes.push(Mode::Expr { braces: 0, in_template: true });
                    i += len;
                } else {
                    let start = i;
                    while i < chars.len() && template_open(&chars, i).is_none() {
                        if at(i, "$${") || at(i, "%%{") {
                            i += 2;
                        }
                        let newline = chars[i] == '\n';
                        i += 1;
                        if newline {
                            break;
                        }
                    }
                    push(&mut tokens, Kind::StringLit, chars[start..i].iter().collect(), &mut spaces);
                }
            }
            Mode::Expr { braces, in_template } => {
                let c = chars[i];
                let one = |kind| (kind, 1);
                let (kind, len) = match c {
                    ' ' | '\t' => {
                        spaces += 1;
                        i += 1;
                        continue;
                    }
                    '\r' if chars.get(i + 1) == Some(&'\n') => (Kind::Newline, 2),
                    '\n' => one(Kind::Newline),
                    '#' => (Kind::Comment, line_comment_len(&chars, i)),
                    '/' if chars.get(i + 1) == Some(&'/') => (Kind::Comment, line_comment_len(&chars, i)),
                    '/' if chars.get(i + 1) == Some(&'*') => {
                        let end = (i + 2..chars.len().saturating_sub(1)).find(|&k| chars[k] == '*' && chars[k + 1] == '/');
                        (Kind::Comment, end.map_or(chars.len() - i, |e| e + 2 - i))
                    }
                    '"' => {
                        push(&mut tokens, Kind::OQuote, "\"".into(), &mut spaces);
                        modes.push(Mode::Quoted);
                        i += 1;
                        continue;
                    }
                    '<' if chars.get(i + 1) == Some(&'<') => match heredoc_open(&chars, i) {
                        Some((len, marker)) => {
                            push(&mut tokens, Kind::OHeredoc, chars[i..i + len].iter().collect(), &mut spaces);
                            modes.push(Mode::Heredoc { marker });
                            i += len;
                            continue;
                        }
                        None => (Kind::Operator, 1),
                    },
                    '~' if *in_template && *braces == 0 && chars.get(i + 1) == Some(&'}') => {
                        push(&mut tokens, Kind::TemplateSeqEnd, "~}".into(), &mut spaces);
                        modes.pop();
                        i += 2;
                        continue;
                    }
                    '}' if *in_template && *braces == 0 => {
                        push(&mut tokens, Kind::TemplateSeqEnd, "}".into(), &mut spaces);
                        modes.pop();
                        i += 1;
                        continue;
                    }
                    '{' => {
                        *braces += 1;
                        one(Kind::OBrace)
                    }
                    '}' => {
                        *braces = braces.saturating_sub(1);
                        one(Kind::CBrace)
                    }
                    '[' => one(Kind::OBrack),
                    ']' => one(Kind::CBrack),
                    '(' => one(Kind::OParen),
                    ')' => one(Kind::CParen),
                    ',' => one(Kind::Comma),
                    '?' => one(Kind::Question),
                    ':' => one(Kind::Colon),
                    '.' if at(i, "...") => (Kind::Ellipsis, 3),
                    '.' if !chars.get(i + 1).is_some_and(char::is_ascii_digit) => one(Kind::Dot),
                    '=' if at(i, "==") || at(i, "=>") => (Kind::Operator, 2),
                    '=' => one(Kind::Equal),
                    '!' if at(i, "!=") => (Kind::Operator, 2),
                    '!' => one(Kind::Bang),
                    '<' | '>' if chars.get(i + 1) == Some(&'=') => (Kind::Operator, 2),
                    '&' if at(i, "&&") => (Kind::Operator, 2),
                    '|' if at(i, "||") => (Kind::Operator, 2),
                    '-' => one(Kind::Minus),
                    '+' | '*' | '/' | '%' | '<' | '>' => one(Kind::Operator),
                    c if c.is_ascii_digit() || c == '.' => {
                        let mut k = i;
                        while k < chars.len() && (chars[k].is_ascii_alphanumeric() || chars[k] == '.' || ((chars[k] == '+' || chars[k] == '-') && matches!(chars[k - 1], 'e' | 'E'))) {
                            k += 1;
                        }
                        (Kind::Number, k - i)
                    }
                    c if c.is_alphabetic() || c == '_' => {
                        let mut k = i;
                        while k < chars.len() && (chars[k].is_alphanumeric() || chars[k] == '_' || chars[k] == '-') {
                            k += 1;
                        }
                        (Kind::Ident, k - i)
                    }
                    _ => one(Kind::Other),
                };
                push(&mut tokens, kind, chars[i..i + len].iter().collect(), &mut spaces);
                i += len;
            }
        }
    }
    tokens
}

fn push(tokens: &mut Vec<Token>, kind: Kind, text: String, spaces: &mut usize) {
    tokens.push(Token { kind, text, spaces: *spaces });
    *spaces = 0;
}

/// `${`, `${~`, `%{` or `%{~` at `i`, unless escaped.
fn template_open(chars: &[char], i: usize) -> Option<(Kind, usize)> {
    let kind = match (chars.get(i), chars.get(i + 1)) {
        (Some('$'), Some('{')) => Kind::TemplateInterp,
        (Some('%'), Some('{')) => Kind::TemplateControl,
        _ => return None,
    };
    if i > 0 && chars[i - 1] == chars[i] {
        return None;
    }
    Some((kind, if chars.get(i + 2) == Some(&'~') { 3 } else { 2 }))
}

/// `<<EOT` or `<<-EOT` and the newline after it: its length and the marker.
fn heredoc_open(chars: &[char], i: usize) -> Option<(usize, String)> {
    let mut k = i + 2;
    if chars.get(k) == Some(&'-') {
        k += 1;
    }
    let start = k;
    while k < chars.len() && (chars[k].is_alphanumeric() || chars[k] == '_') {
        k += 1;
    }
    if k == start {
        return None;
    }
    let marker: String = chars[start..k].iter().collect();
    match chars.get(k) {
        Some('\n') => Some((k + 1 - i, marker)),
        Some('\r') if chars.get(k + 1) == Some(&'\n') => Some((k + 2 - i, marker)),
        _ => None,
    }
}

/// A `#` or `//` comment runs to the end of the line and takes the newline.
fn line_comment_len(chars: &[char], i: usize) -> usize {
    chars[i..].iter().position(|&c| c == '\n').map_or(chars.len() - i, |p| p + 1)
}

fn bracket_change(t: &Token) -> i32 {
    match t.kind {
        Kind::OBrace | Kind::OBrack | Kind::OParen | Kind::TemplateInterp | Kind::TemplateControl => 1,
        Kind::CBrace | Kind::CBrack | Kind::CParen | Kind::TemplateSeqEnd => -1,
        _ => 0,
    }
}

fn ends_line(t: &Token) -> bool {
    t.kind == Kind::Newline || (t.kind == Kind::Comment && t.text.ends_with('\n'))
}

/// One line as the formatter sees it: what precedes the `=`, the `=` and
/// its value, and a trailing comment. Indices into the token list.
struct Line {
    lead: std::ops::Range<usize>,
    assign: Option<std::ops::Range<usize>>,
    comment: Option<usize>,
}

/// Format HCL source. Only whitespace changes; the text must parse.
pub fn format(src: &str) -> String {
    let mut tokens = lex(src);
    let lines = lines(&tokens);
    indent(&mut tokens, &lines);
    space(&mut tokens, &lines);
    align(&mut tokens, &lines);
    tokens.iter().map(|t| format!("{}{}", " ".repeat(t.spaces), t.text)).collect()
}

fn lines(tokens: &[Token]) -> Vec<Line> {
    let mut out = Vec::new();
    let mut start = 0;
    for (i, t) in tokens.iter().enumerate() {
        if ends_line(t) {
            out.push(start..i + 1);
            start = i + 1;
        }
    }
    if start < tokens.len() {
        out.push(start..tokens.len());
    }
    out.into_iter()
        .map(|range| {
            let mut lead = range.clone();
            let mut comment = None;
            if lead.len() > 1 && tokens[lead.end - 1].kind == Kind::Comment {
                comment = Some(lead.end - 1);
                lead.end -= 1;
            }
            let mut assign = None;
            if let Some(eq) = lead.clone().skip(1).find(|&i| tokens[i].kind == Kind::Equal) {
                let mut net = 0;
                for t in &tokens[eq..lead.end] {
                    net += bracket_change(t);
                    if t.kind == Kind::OHeredoc {
                        break;
                    }
                }
                if net == 0 {
                    assign = Some(eq..lead.end);
                    lead.end = eq;
                }
            }
            Line { lead, assign, comment }
        })
        .collect()
}

fn indent(tokens: &mut [Token], lines: &[Line]) {
    let mut indents: Vec<i32> = Vec::new();
    for line in lines {
        if line.lead.is_empty() {
            continue;
        }
        if tokens[line.lead.start].kind == Kind::Newline {
            tokens[line.lead.start].spaces = 0;
            continue;
        }
        let mut net = 0;
        for range in [Some(line.lead.clone()), line.assign.clone()].into_iter().flatten() {
            for t in &tokens[range] {
                net += bracket_change(t);
                if t.kind == Kind::OHeredoc {
                    break;
                }
            }
        }
        if net > 0 {
            tokens[line.lead.start].spaces = 2 * indents.len();
            indents.push(net);
            continue;
        }
        let mut closed = -net;
        while closed > 0 {
            let Some(last) = indents.last_mut() else { break };
            if closed >= *last {
                closed -= *last;
                indents.pop();
            } else {
                *last -= closed;
                closed = 0;
            }
        }
        tokens[line.lead.start].spaces = 2 * indents.len();
    }
}

fn space(tokens: &mut [Token], lines: &[Line]) {
    for line in lines {
        for (n, range) in [Some(line.lead.clone()), line.assign.clone()].into_iter().flatten().enumerate() {
            if n == 1 {
                // The `=` is one space from what it assigns to; align() may widen it.
                tokens[range.start].spaces = 1;
            }
            for i in range.clone() {
                if i + 1 >= range.end {
                    break;
                }
                let before = (i > range.start).then(|| tokens[i - 1].clone());
                tokens[i + 1].spaces = usize::from(space_after(&tokens[i], before.as_ref(), &tokens[i + 1]));
            }
        }
    }
}

fn space_after(subject: &Token, before: Option<&Token>, after: &Token) -> bool {
    use Kind::*;
    let (s, a) = (subject.kind, after.kind);
    match () {
        _ if a == Newline => false,
        _ if s == Ident && a == OParen => false,
        _ if s == Dot || a == Dot => false,
        _ if a == Comma || a == Ellipsis => false,
        _ if s == Comma => true,
        _ if s == Minus => {
            // Negation takes no space; subtraction does. Negation follows
            // something that cannot end an expression.
            !matches!(
                before.map(|b| b.kind),
                None | Some(OParen | OBrace | OBrack | Equal | Colon | Comma | Question | Minus | Operator | Bang)
            )
        }
        _ if s == Bang => false,
        _ if matches!(s, QuotedLit | StringLit | OQuote | OHeredoc) || matches!(a, QuotedLit | StringLit | CQuote | CHeredoc) => false,
        _ if s == OBrace || a == CBrace => !(s == OBrace && a == CBrace),
        _ if matches!(s, TemplateInterp | TemplateControl) && a == OBrace => true,
        _ if s == CBrace && a == TemplateSeqEnd => true,
        _ if s == TemplateSeqEnd && matches!(a, TemplateInterp | TemplateControl) => false,
        _ if bracket_change(subject) > 0 => false,
        _ if bracket_change(after) < 0 => false,
        _ => true,
    }
}

fn columns(tokens: &[Token], range: &std::ops::Range<usize>) -> usize {
    tokens[range.clone()].iter().map(|t| t.spaces + t.text.chars().count()).sum()
}

fn align(tokens: &mut [Token], lines: &[Line]) {
    let mut chain: Vec<&Line> = Vec::new();
    let close = |tokens: &mut [Token], chain: &mut Vec<&Line>, width: &dyn Fn(&[Token], &Line) -> usize, first: &dyn Fn(&Line) -> usize| {
        let max = chain.iter().map(|l| width(tokens, l)).max().unwrap_or(0);
        for l in chain.drain(..) {
            let w = width(tokens, l);
            tokens[first(l)].spaces = max - w + 1;
        }
    };
    let lead_width = |t: &[Token], l: &Line| columns(t, &l.lead);
    for line in lines {
        if line.assign.is_some() {
            chain.push(line);
        } else {
            close(tokens, &mut chain, &lead_width, &|l| l.assign.clone().unwrap().start);
        }
    }
    close(tokens, &mut chain, &lead_width, &|l| l.assign.clone().unwrap().start);

    let full_width = |t: &[Token], l: &Line| columns(t, &l.lead) + l.assign.as_ref().map_or(0, |a| columns(t, a));
    for line in lines {
        if line.comment.is_some() {
            chain.push(line);
        } else {
            close(tokens, &mut chain, &full_width, &|l| l.comment.unwrap());
        }
    }
    close(tokens, &mut chain, &full_width, &|l| l.comment.unwrap());
}

/// `text` with the unit added to every bare number given to a duration
/// attribute: `ttl = 30` becomes `ttl = "30s"`. Keel refuses a bare number
/// there, so this is the one change `validate --write` makes besides
/// formatting.
pub fn add_duration_units(text: &str) -> Result<String> {
    use hcl_edit::structure::{Body, Structure};
    use hcl_edit::Span;

    fn walk(body: &Body, out: &mut Vec<(std::ops::Range<usize>, String)>) {
        for structure in body.iter() {
            match structure {
                Structure::Attribute(attr) if super::DURATION_ATTRIBUTES.contains(&attr.key.as_str()) => {
                    if let hcl_edit::expr::Expression::Number(n) = &attr.value {
                        if let (Some(v), Some(span)) = (n.value().as_u64(), n.span()) {
                            out.push((span, format!("\"{v}s\"")));
                        }
                    }
                }
                Structure::Attribute(_) => {}
                Structure::Block(block) => walk(&block.body, out),
            }
        }
    }
    let body = hcl_edit::parser::parse_body(text).map_err(|e| anyhow::anyhow!("{e}"))?;
    let mut edits = Vec::new();
    walk(&body, &mut edits);
    let mut out = text.to_owned();
    for (span, replacement) in edits.into_iter().rev() {
        out.replace_range(span, &replacement);
    }
    Ok(out)
}

/// A unified diff of two versions of a file, three lines of context.
pub fn diff(name: &str, old: &str, new: &str) -> String {
    let a: Vec<&str> = old.split_inclusive('\n').collect();
    let b: Vec<&str> = new.split_inclusive('\n').collect();
    // Longest common subsequence, from the end.
    let mut lcs = vec![vec![0u32; b.len() + 1]; a.len() + 1];
    for i in (0..a.len()).rev() {
        for j in (0..b.len()).rev() {
            lcs[i][j] = if a[i] == b[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    // (old line, new line, tag) for every line of the edit script.
    let mut ops: Vec<(usize, usize, char)> = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < a.len() || j < b.len() {
        if i < a.len() && j < b.len() && a[i] == b[j] {
            ops.push((i, j, ' '));
            i += 1;
            j += 1;
        } else if i < a.len() && (j == b.len() || lcs[i + 1][j] >= lcs[i][j + 1]) {
            ops.push((i, j, '-'));
            i += 1;
        } else {
            ops.push((i, j, '+'));
            j += 1;
        }
    }
    let changed: Vec<usize> = ops.iter().enumerate().filter(|(_, o)| o.2 != ' ').map(|(k, _)| k).collect();
    if changed.is_empty() {
        return String::new();
    }
    let path = name.trim_start_matches('/');
    let mut out = format!("--- old/{path}\n+++ new/{path}\n");
    let mut k = 0;
    while k < changed.len() {
        let start = changed[k].saturating_sub(3);
        let mut end = changed[k] + 1;
        while k < changed.len() && changed[k] <= end + 5 {
            end = changed[k] + 1;
            k += 1;
        }
        let end = (end + 3).min(ops.len());
        let hunk = &ops[start..end];
        let old_len = hunk.iter().filter(|o| o.2 != '+').count();
        let new_len = hunk.iter().filter(|o| o.2 != '-').count();
        out.push_str(&format!("@@ -{},{old_len} +{},{new_len} @@\n", hunk[0].0 + 1, hunk[0].1 + 1));
        for &(i, j, tag) in hunk {
            let line = if tag == '+' { b[j] } else { a[i] };
            out.push(tag);
            out.push_str(line);
            if !line.ends_with('\n') {
                out.push('\n');
            }
        }
    }
    out
}

/// Every `*.hcl` file below `dir`, sorted.
fn hcl_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).with_context(|| format!("cannot read {}", dir.display()))? {
        let path = entry?.path();
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        if name.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            out.extend(hcl_files(&path)?);
        } else if name.ends_with(".hcl") {
            out.push(path);
        }
    }
    out.sort();
    Ok(out)
}

/// What `keel config fmt` does with what it finds.
pub struct FmtOptions {
    pub write: bool,
    pub list: bool,
    pub diff: bool,
    pub check: bool,
}

/// Exit status of `keel config fmt --check` when a file is not formatted.
pub const NOT_FORMATTED: i32 = 3;

/// Format the given files and directories (recursively). Returns the exit
/// status: 0, [`NOT_FORMATTED`] under `check`, or 1 when a file does not
/// parse.
pub fn fmt_command(paths: &[PathBuf], opts: &FmtOptions) -> Result<i32> {
    let mut files = Vec::new();
    for p in paths {
        if p.is_dir() {
            files.extend(hcl_files(p)?);
        } else {
            files.push(p.clone());
        }
    }
    let mut status = 0;
    for file in files {
        let name = file.display().to_string();
        let text = std::fs::read_to_string(&file).with_context(|| format!("cannot read {name}"))?;
        if let Err(e) = super::hcl::parse(&super::hcl::Source { name: name.clone(), text: &text }) {
            eprintln!("{e:#}");
            status = 1;
            continue;
        }
        let formatted = format(&text);
        if formatted == text {
            continue;
        }
        if opts.list {
            println!("{name}");
        }
        if opts.diff {
            print!("{}", diff(&name, &text, &formatted));
        }
        if opts.check {
            status = status.max(NOT_FORMATTED);
        } else if opts.write {
            std::fs::write(&file, &formatted).with_context(|| format!("cannot write {name}"))?;
        }
    }
    Ok(status)
}

/// Validate the node file and a config directory. Suggested changes —
/// formatting, units on bare durations — are printed as diffs, or applied
/// first with `write`. Returns the exit status: 0 valid, 1 not.
pub fn validate_command(node_path: &str, dir: Option<&Path>, write: bool) -> Result<i32> {
    let node_file = super::read_node_file(node_path)?;
    let dir = match dir {
        Some(d) => d.to_path_buf(),
        None => match super::config_dir(node_path, &node_file) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("{e:#}");
                return Ok(1);
            }
        },
    };
    let mut targets: Vec<PathBuf> = hcl_files(&dir)?;
    if Path::new(node_path).exists() {
        targets.insert(0, PathBuf::from(node_path));
    }
    for file in &targets {
        let name = file.display().to_string();
        let text = std::fs::read_to_string(file).with_context(|| format!("cannot read {name}"))?;
        let Ok(fixed) = add_duration_units(&text).map(|t| format(&t)) else { continue };
        if fixed == text {
            continue;
        }
        if write {
            std::fs::write(file, &fixed).with_context(|| format!("cannot write {name}"))?;
            println!("{name}: rewritten");
        } else {
            print!("{}", diff(&name, &text, &fixed));
        }
    }
    let node_file = super::read_node_file(node_path)?;
    let files = super::read_config_dir(&dir)?;
    match super::build(node_path, &node_file, &files, Some(&dir)) {
        Ok(_) => {
            println!("{}: the configuration is valid", dir.display());
            Ok(0)
        }
        Err(e) => {
            eprintln!("{e:#}");
            Ok(1)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indents_spaces_and_aligns_like_the_reference_formatter() {
        let src = "pool \"web\" {\nalgorithm=\"round_robin\"\n  backend \"10.0.0.1:80\" { weight=2 }\n    backend \"10.0.0.2:80\" {   }\n}\n";
        let want = "pool \"web\" {\n  algorithm = \"round_robin\"\n  backend \"10.0.0.1:80\" { weight = 2 }\n  backend \"10.0.0.2:80\" {}\n}\n";
        assert_eq!(format(src), want);
        assert_eq!(format(want), want, "formatting is stable");
    }

    #[test]
    fn aligns_equals_and_comments_in_runs_of_attributes() {
        let src = "keel {\n  workers = 2 # two\n  user = \"nobody\" # who\n\n  control_user=\"x\"\n}\n";
        let want = "keel {\n  workers = 2        # two\n  user    = \"nobody\" # who\n\n  control_user = \"x\"\n}\n";
        assert_eq!(format(src), want);
    }

    #[test]
    fn templates_lists_objects_and_negation() {
        let src = "locals {\n  a = \"${ local.b }:${local.c}\"\n  l = [ 1,2 , -3 ]\n  o = {x=1, y = [ \"a\" ]}\n  n = 1 - 2\n}\n";
        let want = "locals {\n  a = \"${local.b}:${local.c}\"\n  l = [1, 2, -3]\n  o = { x = 1, y = [\"a\"] }\n  n = 1 - 2\n}\n";
        assert_eq!(format(src), want);
    }

    #[test]
    fn multi_line_values_and_heredocs_keep_their_content() {
        let src = "vhost \"*\" {\ndefault_action {\nstatus = 503\nbody = <<EOT\n  Down for\n    maintenance\nEOT\n}\n}\nx {\n  list = [\n\"a\",\n  ]\n}\n";
        let want = "vhost \"*\" {\n  default_action {\n    status = 503\n    body   = <<EOT\n  Down for\n    maintenance\nEOT\n  }\n}\nx {\n  list = [\n    \"a\",\n  ]\n}\n";
        assert_eq!(format(src), want);
    }

    #[test]
    fn comments_blank_lines_and_trailing_space() {
        let src = "# top   \n\n\n// c\npool \"p\" {   \n  /* block */ passive {}\n}";
        let want = "# top   \n\n\n// c\npool \"p\" {\n  /* block */ passive {}\n}";
        assert_eq!(format(src), want);
    }

    #[test]
    fn bare_duration_numbers_get_a_unit() {
        let src = "pool \"p\" {\n  passive {\n    eject_for = 30\n    failures  = 3\n  }\n}\nvhost \"*\" {\n  cache {\n    ttl = 60\n  }\n}\n";
        let fixed = add_duration_units(src).unwrap();
        assert!(fixed.contains("eject_for = \"30s\"") && fixed.contains("failures  = 3") && fixed.contains("ttl = \"60s\""), "{fixed}");
    }

    #[test]
    fn unified_diff() {
        let d = diff("a.hcl", "x {\nb=1\n}\n", "x {\n  b = 1\n}\n");
        assert_eq!(d, "--- old/a.hcl\n+++ new/a.hcl\n@@ -1,3 +1,3 @@\n x {\n-b=1\n+  b = 1\n }\n");
        assert_eq!(diff("a.hcl", "same\n", "same\n"), "");
    }
}
