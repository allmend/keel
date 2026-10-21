//! Reading the configuration language: HCL native syntax. Named things are
//! labeled blocks, `locals` are shared by every file of a namespace, and a
//! value is a literal, a reference to a local, or a string template built
//! from those. Functions, operators, conditionals, `for` expressions and
//! template directives are refused.
//!
//! The files become one JSON value shaped like [`super::Config`], plus the
//! position of everything in it, so an error found while deserializing it
//! names the file, line and column it came from.

use std::collections::HashMap;
use std::fmt;

use anyhow::Result;
use hcl_edit::expr::{Expression, ObjectKey, TraversalOperator, UnaryOperator};
use hcl_edit::structure::{Block, Body, Structure};
use hcl_edit::template::{Element, Template};
use hcl_edit::Span;
use serde_json::{Map, Value};

/// A place in a file: `path:line:column`, both one-based.
#[derive(Debug, Clone, PartialEq)]
pub struct Pos {
    pub file: String,
    pub line: usize,
    pub column: usize,
}

impl fmt::Display for Pos {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}:{}", self.file, self.line, self.column)
    }
}

/// One file to read: the name errors use, and its text.
pub struct Source<'a> {
    pub name: String,
    pub text: &'a str,
}

impl Source<'_> {
    fn pos(&self, offset: usize) -> Pos {
        let before = &self.text[..offset.min(self.text.len())];
        let line_start = before.rfind('\n').map_or(0, |i| i + 1);
        Pos { file: self.name.clone(), line: before.matches('\n').count() + 1, column: before[line_start..].chars().count() + 1 }
    }

    fn at(&self, span: Option<std::ops::Range<usize>>) -> Pos {
        self.pos(span.map_or(0, |s| s.start))
    }
}

/// An error at a place in a file.
pub fn error_at(pos: &Pos, msg: impl fmt::Display) -> anyhow::Error {
    anyhow::anyhow!("{pos}: {msg}")
}

/// Parse one file; a syntax error names where it is.
pub fn parse(source: &Source) -> Result<Body> {
    hcl_edit::parser::parse_body(source.text).map_err(|e| {
        let loc = e.location();
        let pos = Pos { file: source.name.clone(), line: loc.line(), column: loc.column() };
        error_at(&pos, e.message())
    })
}

/// The files read into one value, and where each part of it came from.
pub struct Document {
    pub value: Map<String, Value>,
    spans: HashMap<String, Pos>,
}

impl Document {
    /// Where the value at `path` (or the nearest thing containing it) was
    /// written.
    pub fn locate(&self, path: &str) -> Option<&Pos> {
        let mut path = path;
        loop {
            if let Some(pos) = self.spans.get(path) {
                return Some(pos);
            }
            path = &path[..path.rfind(['.', '['])?];
        }
    }
}

/// Which file a block or attribute is in: the node file or the config
/// directory (its root file or another one). They hold different sections.
#[derive(Clone, Copy, PartialEq)]
pub enum Role {
    Node,
    Root,
    Other,
}

/// Sections of the node file: how this node is reached and run.
const NODE_SECTIONS: [&str; 4] = ["keel", "cluster", "control", "metrics"];
/// `keel` settings that describe the node rather than the load balancer.
pub const NODE_KEEL_KEYS: [&str; 7] = ["workers", "user", "group", "control_user", "control_socket", "state_dir", "config_dir"];
/// Sections only the root file of the config directory may hold; the other
/// files add pools, vhosts, listeners and certificates.
const ROOT_ONLY_SECTIONS: [&str; 4] = ["keel", "access_log", "acme", "cache"];

#[derive(Clone, Copy, PartialEq)]
enum Many {
    /// At most one; the labels become fields.
    One,
    /// Appended to a list; the labels become fields of each entry.
    List,
    /// A map keyed by the one label.
    Map,
}

/// A block the configuration knows: where it may appear (the context of
/// its parent), the field it fills, its labels, and the context of its body.
struct BlockSpec {
    parent: &'static str,
    ident: &'static str,
    field: &'static str,
    labels: &'static [&'static str],
    many: Many,
    body: &'static str,
}

const fn spec(parent: &'static str, ident: &'static str, field: &'static str, labels: &'static [&'static str], many: Many, body: &'static str) -> BlockSpec {
    BlockSpec { parent, ident, field, labels, many, body }
}

const BLOCKS: &[BlockSpec] = &[
    spec("", "keel", "keel", &[], Many::One, "keel"),
    spec("", "cluster", "cluster", &[], Many::One, "cluster"),
    spec("", "control", "control", &[], Many::One, "control"),
    spec("", "metrics", "metrics", &[], Many::One, "metrics"),
    spec("", "access_log", "access_log", &[], Many::One, "access_log"),
    spec("", "acme", "acme", &[], Many::One, "acme"),
    spec("", "cache", "cache", &[], Many::One, "cache"),
    spec("", "listener", "listeners", &["name"], Many::List, "listener"),
    spec("", "pool", "pools", &["name"], Many::Map, "pool"),
    spec("", "vhost", "vhosts", &["host"], Many::List, "vhost"),
    spec("", "certificate", "certificates", &["host"], Many::List, "certificate"),
    spec("pool", "health_check", "health_check", &["type"], Many::One, "health_check"),
    spec("pool", "passive", "passive", &[], Many::One, "passive"),
    spec("pool", "backend", "backends", &["address"], Many::List, "backend"),
    spec("vhost", "route", "routes", &["path"], Many::List, "route"),
    spec("vhost", "tls", "tls", &[], Many::One, "tls"),
    spec("vhost", "forwarded_headers", "forwarded_headers", &[], Many::One, "forwarded_headers"),
    spec("vhost", "default_action", "default_action", &[], Many::One, "default_action"),
    spec("vhost", "cache", "cache", &[], Many::One, "vhost_cache"),
    spec("route", "cache", "cache", &[], Many::One, "vhost_cache"),
    spec("acme", "issuer", "issuers", &["name"], Many::Map, "issuer"),
    spec("cache", "disk", "disk", &[], Many::One, "disk"),
    spec("control", "remote", "remote", &[], Many::One, "remote"),
];

/// A `local.<name>`: its expression and the file it is in.
struct Local<'a> {
    expr: &'a Expression,
    source: usize,
    pos: Pos,
}

/// Locals of one namespace, evaluated on first use.
struct Locals<'a> {
    defined: HashMap<String, Local<'a>>,
    values: HashMap<String, Value>,
    /// Locals being evaluated, to name a cycle.
    stack: Vec<String>,
}

struct Reader<'a> {
    sources: &'a [Source<'a>],
    doc: Document,
    /// Which file each root `keel` block came from, to merge the node
    /// file's into the config directory's.
    keel_from: Option<usize>,
}

/// Read the node file and the config directory into one document. `node` is
/// `None` to read the config directory alone, `config` empty to read the
/// node file alone. The config directory's root file comes first in `config`.
pub fn read(node: Option<&Source>, config: &[Source]) -> Result<Document> {
    let mut sources: Vec<Source> = Vec::new();
    if let Some(n) = node {
        sources.push(Source { name: n.name.clone(), text: n.text });
    }
    let node_count = sources.len();
    sources.extend(config.iter().map(|s| Source { name: s.name.clone(), text: s.text }));
    let bodies = sources.iter().map(parse).collect::<Result<Vec<_>>>()?;

    let mut node_locals = Locals::collect(&bodies[..node_count], &sources, 0)?;
    let mut config_locals = Locals::collect(&bodies[node_count..], &sources, node_count)?;

    let mut reader = Reader { sources: &sources, doc: Document { value: Map::new(), spans: HashMap::new() }, keel_from: None };
    for (i, body) in bodies.iter().enumerate() {
        let (role, locals) = match i {
            i if i < node_count => (Role::Node, &mut node_locals),
            i if i == node_count => (Role::Root, &mut config_locals),
            _ => (Role::Other, &mut config_locals),
        };
        reader.root(body, i, role, locals)?;
    }
    Ok(reader.doc)
}

impl<'a> Locals<'a> {
    fn collect(bodies: &'a [Body], sources: &[Source], first: usize) -> Result<Locals<'a>> {
        let mut defined: HashMap<String, Local> = HashMap::new();
        for (i, body) in bodies.iter().enumerate() {
            let source = &sources[first + i];
            for block in body.blocks().filter(|b| b.has_ident("locals")) {
                if !block.labels.is_empty() {
                    return Err(error_at(&source.at(block.span()), "a `locals` block has no labels"));
                }
                for structure in block.body.iter() {
                    let Structure::Attribute(attr) = structure else {
                        return Err(error_at(&source.at(structure.span()), "a `locals` block holds attributes only"));
                    };
                    let name = attr.key.as_str().to_owned();
                    let pos = source.at(attr.key.span());
                    if let Some(earlier) = defined.get(&name) {
                        return Err(error_at(&pos, format!("local.{name} is already defined at {}", earlier.pos)));
                    }
                    defined.insert(name, Local { expr: &attr.value, source: first + i, pos });
                }
            }
        }
        Ok(Locals { defined, values: HashMap::new(), stack: Vec::new() })
    }

    fn get(&mut self, name: &str, sources: &[Source], used_at: &Pos) -> Result<Value> {
        if let Some(v) = self.values.get(name) {
            return Ok(v.clone());
        }
        let Some(local) = self.defined.get(name) else {
            return Err(error_at(used_at, format!("local.{name} is not defined")));
        };
        if let Some(start) = self.stack.iter().position(|n| n == name) {
            let mut chain: Vec<String> = self.stack[start..].iter().map(|n| format!("local.{n}")).collect();
            chain.push(format!("local.{name}"));
            return Err(error_at(&local.pos, format!("locals refer to each other in a cycle: {}", chain.join(" -> "))));
        }
        let (expr, source) = (local.expr, local.source);
        self.stack.push(name.to_owned());
        let value = eval(expr, &sources[source], sources, self);
        self.stack.pop();
        let value = value?;
        self.values.insert(name.to_owned(), value.clone());
        Ok(value)
    }
}

/// Why an expression is not a value Keel accepts.
const NOT_A_VALUE: &str = "values are literals, local.<name> references and string templates";

fn eval(expr: &Expression, source: &Source, sources: &[Source], locals: &mut Locals) -> Result<Value> {
    let refuse = |what: &str| Err(error_at(&source.at(expr.span()), format!("{what} not supported: {NOT_A_VALUE}")));
    Ok(match expr {
        Expression::Null(_) => Value::Null,
        Expression::Bool(b) => Value::Bool(*b.value()),
        Expression::Number(n) => number(n.value()),
        Expression::String(s) => Value::String(s.value().clone()),
        Expression::Array(items) => {
            Value::Array(items.iter().map(|e| eval(e, source, sources, locals)).collect::<Result<_>>()?)
        }
        Expression::Object(object) => {
            let mut map = Map::new();
            for (key, value) in object.iter() {
                let key = match key {
                    ObjectKey::Ident(ident) => ident.as_str().to_owned(),
                    ObjectKey::Expression(e) => match eval(e, source, sources, locals)? {
                        Value::String(s) => s,
                        _ => return Err(error_at(&source.at(e.span()), "an object key must be a string")),
                    },
                };
                map.insert(key, eval(value.expr(), source, sources, locals)?);
            }
            Value::Object(map)
        }
        Expression::StringTemplate(t) => template(t, source, sources, locals)?,
        Expression::HeredocTemplate(h) => template(&h.template, source, sources, locals)?,
        Expression::Parenthesis(p) => eval(p.inner(), source, sources, locals)?,
        Expression::Variable(v) => {
            return Err(error_at(
                &source.at(v.span()),
                format!("`{}` is not a value: quote strings, and refer to locals as local.<name>", v.as_str()),
            ))
        }
        Expression::Traversal(t) => {
            let pos = source.at(t.span());
            let mut ops = t.operators.iter();
            let name = match (&t.expr, ops.next().map(|o| o.value())) {
                (Expression::Variable(v), Some(TraversalOperator::GetAttr(name))) if v.as_str() == "local" => name.as_str(),
                _ => return Err(error_at(&pos, format!("only local.<name> can be referred to: {NOT_A_VALUE}"))),
            };
            let mut value = locals.get(name, sources, &pos)?;
            for op in ops {
                value = match op.value() {
                    TraversalOperator::GetAttr(attr) => value.get(attr.as_str()).cloned(),
                    TraversalOperator::Index(i) => match eval(i, source, sources, locals)? {
                        Value::Number(n) => n.as_u64().and_then(|n| value.get(n as usize)).cloned(),
                        Value::String(k) => value.get(&k).cloned(),
                        _ => None,
                    },
                    TraversalOperator::LegacyIndex(i) => value.get(*i.value() as usize).cloned(),
                    TraversalOperator::AttrSplat(_) | TraversalOperator::FullSplat(_) => return refuse("splat expressions are"),
                }
                .ok_or_else(|| error_at(&pos, format!("local.{name} has no such element")))?;
            }
            value
        }
        Expression::UnaryOp(op) => match (op.operator.value(), eval(&op.expr, source, sources, locals)?) {
            (UnaryOperator::Neg, Value::Number(n)) => match (n.as_i64(), n.as_f64()) {
                (Some(i), _) => Value::from(-i),
                (None, Some(f)) => Value::from(-f),
                _ => return refuse("this number is"),
            },
            _ => return refuse("operators are"),
        },
        Expression::BinaryOp(_) => return refuse("operators are"),
        Expression::Conditional(_) => return refuse("conditionals are"),
        Expression::FuncCall(_) => return refuse("functions are"),
        Expression::ForExpr(_) => return refuse("for expressions are"),
    })
}

fn number(n: &hcl_edit::Number) -> Value {
    if let Some(i) = n.as_u64() {
        Value::from(i)
    } else if let Some(i) = n.as_i64() {
        Value::from(i)
    } else {
        n.as_f64().map_or(Value::Null, Value::from)
    }
}

/// A template is a string; one made of a single interpolation is the value
/// it interpolates, as in HCL.
fn template(t: &Template, source: &Source, sources: &[Source], locals: &mut Locals) -> Result<Value> {
    let elements: Vec<&Element> = t.iter().collect();
    if let [Element::Interpolation(i)] = elements.as_slice() {
        return eval(&i.expr, source, sources, locals);
    }
    let mut out = String::new();
    for element in elements {
        match element {
            Element::Literal(s) => out.push_str(s.value()),
            Element::Interpolation(i) => match eval(&i.expr, source, sources, locals)? {
                Value::String(s) => out.push_str(&s),
                v @ (Value::Number(_) | Value::Bool(_)) => out.push_str(&v.to_string()),
                _ => return Err(error_at(&source.at(i.expr.span()), "only strings, numbers and booleans can be interpolated")),
            },
            Element::Directive(d) => {
                return Err(error_at(&source.at(d.span()), format!("template directives (%{{if}}, %{{for}}) are not supported: {NOT_A_VALUE}")))
            }
        }
    }
    Ok(Value::String(out))
}

impl Reader<'_> {
    fn root(&mut self, body: &Body, index: usize, role: Role, locals: &mut Locals) -> Result<()> {
        let source = &self.sources[index];
        for structure in body.iter() {
            let block = match structure {
                Structure::Attribute(attr) => {
                    return Err(error_at(&source.at(attr.key.span()), format!("unknown attribute `{}`: the top level holds blocks only", attr.key.as_str())))
                }
                Structure::Block(block) if block.has_ident("locals") => continue,
                Structure::Block(block) => block,
            };
            let ident = block.ident.as_str();
            let pos = source.at(block.ident.span());
            let node_file = "the node file";
            match role {
                Role::Node if !NODE_SECTIONS.contains(&ident) && lookup("", ident).is_some() => {
                    return Err(error_at(&pos, format!("`{ident}` belongs in the config directory, not in {node_file}")))
                }
                Role::Root | Role::Other if NODE_SECTIONS.contains(&ident) && ident != "keel" => {
                    return Err(error_at(&pos, format!("`{ident}` belongs in {node_file}")))
                }
                Role::Other if ROOT_ONLY_SECTIONS.contains(&ident) => {
                    return Err(error_at(&pos, format!("`{ident}` may only be set in {}", super::ROOT_FILE)))
                }
                _ => {}
            }
            if ident == "keel" {
                for attr in block.body.attributes() {
                    let key = attr.key.as_str();
                    let node_key = NODE_KEEL_KEYS.contains(&key);
                    let wrong = match role {
                        Role::Node if !node_key => Some(format!("`keel.{key}` belongs in the config directory's {}", super::ROOT_FILE)),
                        Role::Root if node_key => Some(format!("`keel.{key}` belongs in {node_file}")),
                        _ => None,
                    };
                    if let Some(msg) = wrong {
                        return Err(error_at(&source.at(attr.key.span()), msg));
                    }
                }
                // The node file's `keel` and the root file's are one section.
                if self.keel_from.is_some_and(|from| from != index) {
                    let mut keel = match self.doc.value.remove("keel") {
                        Some(Value::Object(m)) => m,
                        _ => Map::new(),
                    };
                    self.body(&block.body, index, "keel", "keel", &[], &mut keel, locals)?;
                    self.doc.value.insert("keel".into(), Value::Object(keel));
                    continue;
                }
                self.keel_from = Some(index);
            }
            let mut root = std::mem::take(&mut self.doc.value);
            let result = self.block(block, index, "", "", &mut root, locals);
            self.doc.value = root;
            result?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn body(&mut self, body: &Body, index: usize, ctx: &str, path: &str, labels: &[&str], obj: &mut Map<String, Value>, locals: &mut Locals) -> Result<()> {
        let source = &self.sources[index];
        for structure in body.iter() {
            match structure {
                Structure::Attribute(attr) => {
                    let key = attr.key.as_str();
                    let pos = source.at(attr.key.span());
                    if BLOCKS.iter().any(|b| b.parent == ctx && (b.ident == key || b.field == key)) {
                        return Err(error_at(&pos, format!("`{key}` is a block here, not an attribute")));
                    }
                    if labels.contains(&key) {
                        return Err(error_at(&pos, format!("`{key}` is set by the block's label")));
                    }
                    if obj.contains_key(key) {
                        return Err(error_at(&pos, format!("`{key}` is set twice")));
                    }
                    let value = eval(&attr.value, source, self.sources, locals)?;
                    self.doc.spans.insert(join(path, key), pos);
                    obj.insert(key.to_owned(), value);
                }
                Structure::Block(block) if block.has_ident("locals") => {
                    return Err(error_at(&source.at(block.ident.span()), "`locals` blocks belong at the top level"));
                }
                Structure::Block(block) => self.block(block, index, ctx, path, obj, locals)?,
            }
        }
        Ok(())
    }

    fn block(&mut self, block: &Block, index: usize, ctx: &str, path: &str, obj: &mut Map<String, Value>, locals: &mut Locals) -> Result<()> {
        let source = &self.sources[index];
        let ident = block.ident.as_str();
        let pos = source.at(block.ident.span());
        let Some(&BlockSpec { field, labels: label_names, many, body: child, .. }) = lookup(ctx, ident) else {
            return Err(error_at(&pos, format!("unknown block `{ident}`")));
        };
        if block.labels.len() != label_names.len() {
            let wanted = match label_names {
                [] => "no labels".to_owned(),
                names => format!("{} label{}: {}", names.len(), if names.len() == 1 { "" } else { "s" }, names.join(", ")),
            };
            return Err(error_at(&pos, format!("a `{ident}` block takes {wanted}")));
        }
        let mut entry = Map::new();
        let path = match many {
            Many::One => {
                if obj.contains_key(field) {
                    return Err(error_at(&pos, format!("only one `{ident}` block is allowed here")));
                }
                join(path, field)
            }
            Many::List => {
                let list = obj.entry(field).or_insert_with(|| Value::Array(Vec::new()));
                let n = list.as_array().map_or(0, Vec::len);
                format!("{}[{n}]", join(path, field))
            }
            Many::Map => {
                let key = block.labels[0].as_str();
                let path = join(&join(path, field), key);
                if let Some(earlier) = obj.get(field).and_then(|m| m.get(key)).and_then(|_| self.doc.spans.get(&path)) {
                    return Err(error_at(&pos, format!("{ident} \"{key}\" is already defined at {earlier}")));
                }
                path
            }
        };
        self.doc.spans.insert(path.clone(), pos);
        let stored: Vec<&str> = if many == Many::Map { Vec::new() } else { label_names.to_vec() };
        for (name, label) in stored.iter().zip(&block.labels) {
            self.doc.spans.insert(join(&path, name), source.at(label.span()));
            entry.insert((*name).to_owned(), Value::String(label.as_str().to_owned()));
        }
        self.body(&block.body, index, child, &path, &stored, &mut entry, locals)?;
        match many {
            Many::One => {
                obj.insert(field.to_owned(), Value::Object(entry));
            }
            Many::List => {
                if let Some(Value::Array(list)) = obj.get_mut(field) {
                    list.push(Value::Object(entry));
                }
            }
            Many::Map => {
                let map = obj.entry(field).or_insert_with(|| Value::Object(Map::new()));
                if let Value::Object(map) = map {
                    map.insert(block.labels[0].as_str().to_owned(), Value::Object(entry));
                }
            }
        }
        Ok(())
    }
}

fn lookup(ctx: &str, ident: &str) -> Option<&'static BlockSpec> {
    BLOCKS.iter().find(|b| b.parent == ctx && b.ident == ident)
}

fn join(path: &str, key: &str) -> String {
    if path.is_empty() {
        key.to_owned()
    } else {
        format!("{path}.{key}")
    }
}

/// The path of a deserialization error in the same form as the document's
/// spans: `pools.web.backends[0].weight`.
pub fn path_of(path: &serde_path_to_error::Path) -> String {
    use serde_path_to_error::Segment;
    let mut out = String::new();
    for segment in path.iter() {
        match segment {
            Segment::Seq { index } => out.push_str(&format!("[{index}]")),
            Segment::Map { key } => {
                if !out.is_empty() {
                    out.push('.');
                }
                out.push_str(key);
            }
            Segment::Enum { .. } | Segment::Unknown => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(text: &str) -> Result<Document> {
        read(None, &[Source { name: "keel.hcl".into(), text }])
    }

    fn err(text: &str) -> String {
        format!("{:#}", doc(text).err().expect("an error"))
    }

    #[test]
    fn labeled_blocks_become_lists_and_maps() {
        let d = doc(r#"
pool "web" {
  backend "10.0.0.1:80" { weight = 2 }
  backend "10.0.0.2:80" {}
}
listener "http" {
  protocol  = "http"
  addresses = ["0.0.0.0"]
  port      = 80
}
vhost "a.test" {
  route "/api/" { pool = "web" }
}
"#)
        .unwrap();
        let v = Value::Object(d.value.clone());
        assert_eq!(v["pools"]["web"]["backends"][0], serde_json::json!({"address": "10.0.0.1:80", "weight": 2}));
        assert_eq!(v["pools"]["web"]["backends"][1], serde_json::json!({"address": "10.0.0.2:80"}));
        assert_eq!(v["listeners"][0], serde_json::json!({"name": "http", "protocol": "http", "addresses": ["0.0.0.0"], "port": 80}));
        assert_eq!(v["vhosts"][0]["routes"][0], serde_json::json!({"path": "/api/", "pool": "web"}));
        assert_eq!(d.locate("pools.web.backends[0].weight").unwrap().to_string(), "keel.hcl:3:27");
        assert_eq!(d.locate("pools.web.backends[1].nothing").unwrap().to_string(), "keel.hcl:4:3");
    }

    #[test]
    fn locals_templates_and_negative_numbers() {
        let d = doc(r#"
locals {
  port = local.base + 0
}
pool "p" {
  passive { failures = local.port }
}
"#);
        assert!(format!("{:#}", d.err().unwrap()).contains("operators are not supported"));

        let d = doc(r#"
locals {
  host = "10.0.0.1"
  addr = "${local.host}:${local.port}"
  port = 80
  hc   = { interval = "5s" }
}
pool "web" {
  backend "10.0.0.1:80" {}
  passive {
    failures  = local.port
    eject_for = "${local.hc.interval}"
  }
}
certificate "a.test" {
  cert = "${local.addr}"
}
"#)
        .unwrap();
        let v = Value::Object(d.value);
        assert_eq!(v["pools"]["web"]["passive"]["failures"], 80);
        assert_eq!(v["pools"]["web"]["passive"]["eject_for"], "5s");
        assert_eq!(v["certificates"][0]["cert"], "10.0.0.1:80");
        let d = doc("pool \"p\" {\n  passive { failures = -3 }\n}\n").unwrap();
        assert_eq!(Value::Object(d.value)["pools"]["p"]["passive"]["failures"], -3);
    }

    #[test]
    fn refuses_what_is_not_a_value() {
        for (text, wanted) in [
            ("pool \"p\" { algorithm = upper(\"x\") }", "functions are not supported"),
            ("pool \"p\" { algorithm = true ? \"a\" : \"b\" }", "conditionals are not supported"),
            ("pool \"p\" { algorithm = [for x in [1] : x] }", "for expressions are not supported"),
            ("pool \"p\" { algorithm = \"%{if true}a%{endif}\" }", "template directives"),
            ("pool \"p\" { algorithm = round_robin }", "`round_robin` is not a value"),
            ("pool \"p\" { algorithm = var.x }", "only local.<name>"),
            ("pool \"p\" { algorithm = local.nope }", "local.nope is not defined"),
        ] {
            let e = err(text);
            assert!(e.contains(wanted), "{text}: {e}");
            assert!(e.starts_with("keel.hcl:1:"), "{e}");
        }
    }

    #[test]
    fn locals_cycles_and_duplicates_are_named() {
        let e = err("locals {\n  a = local.b\n  b = local.a\n}\npool \"p\" { algorithm = local.a }\n");
        assert!(e.contains("cycle: local.a -> local.b -> local.a") || e.contains("cycle: local.b -> local.a -> local.b"), "{e}");

        let sources = [
            Source { name: "keel.hcl".into(), text: "locals {\n  a = 1\n}\n" },
            Source { name: "pools/web.hcl".into(), text: "locals {\n  a = 2\n}\n" },
        ];
        let e = format!("{:#}", read(None, &sources).err().unwrap());
        assert_eq!(e, "pools/web.hcl:2:3: local.a is already defined at keel.hcl:2:3");
    }

    #[test]
    fn locals_are_shared_by_the_config_directory_only() {
        let sources = [
            Source { name: "keel.hcl".into(), text: "locals {\n  port = 8080\n}\n" },
            Source { name: "pools/web.hcl".into(), text: "pool \"web\" {\n  passive { failures = local.port }\n}\n" },
        ];
        let d = read(None, &sources).unwrap();
        assert_eq!(Value::Object(d.value)["pools"]["web"]["passive"]["failures"], 8080);

        let node = Source { name: "node.hcl".into(), text: "keel {\n  workers = local.port\n}\n" };
        let e = format!("{:#}", read(Some(&node), &sources).err().unwrap());
        assert!(e.starts_with("node.hcl:2:13: local.port is not defined"), "{e}");
    }

    #[test]
    fn block_shape_errors() {
        for (text, wanted) in [
            ("pool \"p\" {\n  backends = []\n}", "keel.hcl:2:3: `backends` is a block here, not an attribute"),
            ("pool \"p\" {\n  passive = {}\n}", "keel.hcl:2:3: `passive` is a block here"),
            ("pool \"p\" {\n  backend \"a:1\" { address = \"b:1\" }\n}", "keel.hcl:2:19: `address` is set by the block's label"),
            ("pool \"p\" {\n  backend {}\n}", "keel.hcl:2:3: a `backend` block takes 1 label: address"),
            ("listener \"http\" \"0.0.0.0:80\" {}", "keel.hcl:1:1: a `listener` block takes 1 label: name"),
            ("pool \"p\" {}\npool \"p\" {}", "keel.hcl:2:1: pool \"p\" is already defined at keel.hcl:1:1"),
            ("pool \"p\" {\n  passive {}\n  passive {}\n}", "keel.hcl:3:3: only one `passive` block"),
            ("pool \"p\" {\n  frobnicate {}\n}", "keel.hcl:2:3: unknown block `frobnicate`"),
            ("workers = 2", "keel.hcl:1:1: unknown attribute `workers`"),
            ("cluster {}", "keel.hcl:1:1: `cluster` belongs in the node file"),
            ("keel {\n  workers = 2\n}", "keel.hcl:2:3: `keel.workers` belongs in the node file"),
            ("pool \"p\" {\n  locals {}\n}", "keel.hcl:2:3: `locals` blocks belong at the top level"),
            ("pool \"p\" {", "keel.hcl:1:"),
        ] {
            let e = err(text);
            assert!(e.starts_with(wanted), "{text}:\n{e}");
        }
    }

    #[test]
    fn root_only_and_node_sections() {
        let other = |text: &'static str| {
            let sources = [Source { name: "keel.hcl".into(), text: "" }, Source { name: "x.hcl".into(), text }];
            format!("{:#}", read(None, &sources).err().unwrap())
        };
        assert!(other("cache {}").starts_with("x.hcl:1:1: `cache` may only be set in keel.hcl"));
        assert!(other("keel {}").starts_with("x.hcl:1:1: `keel` may only be set in keel.hcl"));

        let node = Source { name: "node.hcl".into(), text: "keel {\n  workers = 2\n}\nmetrics {\n  address = \"0.0.0.0:1\"\n}\n" };
        let root = Source { name: "keel.hcl".into(), text: "keel {\n  udp_max_flows = 9\n}\n" };
        let d = read(Some(&node), &[root]).unwrap();
        let v = Value::Object(d.value);
        assert_eq!(v["keel"], serde_json::json!({"workers": 2, "udp_max_flows": 9}));
        let node = Source { name: "node.hcl".into(), text: "pool \"p\" {}\n" };
        let e = format!("{:#}", read(Some(&node), &[]).err().unwrap());
        assert!(e.starts_with("node.hcl:1:1: `pool` belongs in the config directory"), "{e}");
        let node = Source { name: "node.hcl".into(), text: "keel {\n  udp_max_flows = 9\n}\n" };
        let e = format!("{:#}", read(Some(&node), &[]).err().unwrap());
        assert!(e.starts_with("node.hcl:2:3: `keel.udp_max_flows` belongs in the config directory's keel.hcl"), "{e}");
    }
}
