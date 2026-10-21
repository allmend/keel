//! `keel config validate`: load a node file and its config directory as a
//! start or reload would. Silent by default: the exit status says whether
//! the files can go live, and an error goes to stderr. Flags add output
//! (`--full`, `--diff`, `--list`) or fix files (`--write`).

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{Config, FileSet};

/// Exit status: the configuration does not load.
pub const INVALID: i32 = 1;
/// Exit status of `--check`: a file is not in canonical format.
pub const NOT_FORMATTED: i32 = 3;

pub struct Options {
    /// The node file; its `keel.config_dir` is the directory loaded.
    pub node: String,
    /// The running node's control socket, for `--diff`.
    pub socket: String,
    pub full: bool,
    pub diff: bool,
    pub list: bool,
    pub write: bool,
    pub check: bool,
    pub color: bool,
    /// One JSON object on stdout instead of text.
    pub json: bool,
}

/// Run `keel config validate`; returns the exit status.
pub fn run(opts: &Options) -> Result<i32> {
    let out = Paint { on: opts.color && std::io::stdout().is_terminal() };
    let err = Paint { on: opts.color && std::io::stderr().is_terminal() };
    let fail = |e: anyhow::Error| {
        if opts.json {
            println!("{}", serde_json::json!({ "valid": false, "error": format!("{e:#}") }));
        } else {
            eprintln!("{}", err.red(&format!("{e:#}")));
        }
        Ok(INVALID)
    };
    let mut report = serde_json::Map::new();

    let node_file = match super::read_node_file(&opts.node) {
        Ok(text) => text,
        Err(e) => return fail(e),
    };
    let dir = match super::config_dir(&opts.node, &node_file) {
        Ok(dir) => dir,
        Err(e) => return fail(e),
    };

    // Formatting, before loading, so --write fixes what the load reads.
    let mut targets = hcl_files(&dir)?;
    if Path::new(&opts.node).exists() {
        targets.insert(0, PathBuf::from(&opts.node));
    }
    let mut unformatted = Vec::new();
    for file in &targets {
        let name = file.display().to_string();
        let text = std::fs::read_to_string(file).with_context(|| format!("cannot read {name}"))?;
        // A file that does not parse is the load's to report.
        let Ok(fixed) = super::fmt::add_duration_units(&text).map(|t| super::fmt::format(&t)) else { continue };
        if fixed == text {
            continue;
        }
        if opts.list && !opts.json {
            println!("{name}");
        }
        if opts.write {
            std::fs::write(file, &fixed).with_context(|| format!("cannot write {name}"))?;
        } else {
            unformatted.push(name);
        }
    }

    let loaded = super::read_node_file(&opts.node)
        .and_then(|node_file| Ok((node_file, super::read_config_dir(&dir).with_context(|| format!("{}", dir.display()))?)));
    let (node_file, files) = match loaded {
        Ok(loaded) => loaded,
        Err(e) => return fail(e),
    };
    let cfg = match super::build(&opts.node, &node_file, &files, Some(&dir)) {
        Ok(cfg) => cfg,
        Err(e) => return fail(e),
    };

    if opts.full {
        let mut loaded = Vec::new();
        if !node_file.is_empty() {
            loaded.push((opts.node.clone(), node_file.as_str()));
        }
        for (path, text) in super::config_files(&files)? {
            loaded.push((dir.join(path).display().to_string(), text));
        }
        if opts.json {
            report.insert("files".into(), loaded.iter().map(|(path, text)| serde_json::json!({ "path": path, "text": text })).collect());
        } else {
            for (path, text) in &loaded {
                print_file(&out, path, text);
            }
        }
    }

    if opts.diff {
        match applied(&opts.socket) {
            Ok((applied_node, applied_files)) => {
                let out = if opts.json { Paint { on: false } } else { out };
                let mut text = diff(&out, &format!("applied/{}", node_name(&opts.node)), &opts.node, &applied_node, &node_file);
                let paths: BTreeSet<&String> = applied_files.keys().chain(files.keys()).collect();
                for path in paths {
                    let old = applied_files.get(path).map_or("", String::as_str);
                    let new = files.get(path).map_or("", String::as_str);
                    text += &diff(&out, &format!("applied/{path}"), &dir.join(path).display().to_string(), old, new);
                }
                if opts.json {
                    report.insert("diff".into(), text.into());
                } else {
                    print!("{text}");
                }
                match super::assemble(&applied_node, &applied_files) {
                    Ok(old) => {
                        let changes = changes(&old, &cfg, applied_node != node_file);
                        if opts.json {
                            let changes = changes.iter().map(|(restart, what)| serde_json::json!({ "restart": restart, "change": what }));
                            report.insert("changes".into(), changes.collect());
                        }
                        for (restart, what) in changes.iter().filter(|_| !opts.json) {
                            let tag = if *restart { out.yellow("restart") } else { out.green("reload ") };
                            println!("{tag} {what}");
                        }
                    }
                    Err(e) => eprintln!("the applied version does not load here: {e:#}"),
                }
            }
            Err(e) => eprintln!("no running node to compare with ({}): {e:#}", opts.socket),
        }
    }

    let status = if opts.check && !unformatted.is_empty() { NOT_FORMATTED } else { 0 };
    if opts.json {
        report.insert("valid".into(), true.into());
        report.insert("unformatted".into(), unformatted.into());
        println!("{}", serde_json::Value::Object(report));
    }
    Ok(status)
}

/// What the running node serves: its node file and config files.
fn applied(socket: &str) -> Result<(String, FileSet)> {
    let mut stream = std::os::unix::net::UnixStream::connect(socket)?;
    keel_control::client::config_applied(&mut stream)
}

fn node_name(path: &str) -> &str {
    Path::new(path).file_name().and_then(|n| n.to_str()).unwrap_or(path)
}

fn print_file(out: &Paint, path: &str, text: &str) {
    println!("{}", out.bold(&format!("# file {path}")));
    print!("{text}");
    if !text.ends_with('\n') {
        println!();
    }
}

/// How the files differ from the applied version, each change marked as
/// applied on reload (`false`) or waiting for a restart (`true`).
/// A change from `old` to `new` the workers cannot apply in place.
pub fn needs_new_workers(old: &Config, new: &Config) -> bool {
    changes(old, new, false).iter().any(|(restart, _)| *restart)
}

pub fn changes(old: &Config, new: &Config, node_file_changed: bool) -> Vec<(bool, String)> {
    let mut out = Vec::new();
    let mut note = |restart: bool, what: String| out.push((restart, what));
    if node_file_changed {
        note(true, "the node file".into());
    }
    if json(&old.keel) != json(&new.keel) {
        note(true, "keel".into());
    }
    let listener = |l: &super::Listener| format!("listener \"{}\"", l.name);
    let listeners = |c: &Config| c.listeners.iter().map(|l| (listener(l), json(l))).collect::<BTreeMap<_, _>>();
    for (name, (o, n)) in pair(&listeners(old), &listeners(new)) {
        if o != n {
            note(true, format!("{name} {}", verb(o.is_some(), n.is_some())));
        }
    }
    if json(&old.access_log) != json(&new.access_log) {
        note(true, "access_log".into());
    }
    if json(&old.cache) != json(&new.cache) {
        note(true, "cache".into());
    }
    for (name, (o, n)) in pair(&old.pools.iter().collect(), &new.pools.iter().collect::<BTreeMap<_, _>>()) {
        let (Some(o), Some(n)) = (o, n) else {
            let what = if o.is_some() { "removed, drains" } else { "added" };
            note(false, format!("pool \"{name}\" {what}"));
            continue;
        };
        for (field, a, b) in [
            ("algorithm", json(&o.algorithm), json(&n.algorithm)),
            ("health_check", json(&o.health_check), json(&n.health_check)),
            ("passive", json(&o.passive), json(&n.passive)),
            ("timeouts", json(&(o.connect_timeout, o.read_timeout, o.write_timeout, o.idle_timeout, o.retry)),
             json(&(n.connect_timeout, n.read_timeout, n.write_timeout, n.idle_timeout, n.retry))),
        ] {
            if a != b {
                note(false, format!("pool \"{name}\": {field}"));
            }
        }
        let backends = |p: &super::Pool| p.backends.iter().map(|b| (b.address.clone(), b.weight)).collect::<BTreeMap<_, _>>();
        for (address, (o, n)) in pair(&backends(o), &backends(n)) {
            match (o, n) {
                (None, Some(_)) => note(false, format!("pool \"{name}\": backend \"{address}\" added")),
                (Some(_), None) => note(false, format!("pool \"{name}\": backend \"{address}\" removed, drains")),
                (Some(a), Some(b)) if a != b => note(false, format!("pool \"{name}\": backend \"{address}\" weight")),
                _ => {}
            }
        }
    }
    if json(&old.vhosts) != json(&new.vhosts) {
        note(false, "vhosts".into());
    }
    if json(&old.certificates) != json(&new.certificates) || old.files != new.files && certificate_files_changed(old, new) {
        note(false, "certificates".into());
    }
    if json(&old.acme) != json(&new.acme) {
        note(false, "acme".into());
    }
    out
}

/// Whether a certificate or key file the config names changed.
fn certificate_files_changed(old: &Config, new: &Config) -> bool {
    let named = |c: &Config| -> Vec<String> {
        let vhosts = c.vhosts.iter().filter_map(|v| v.tls.as_ref()).flat_map(|t| [t.cert.clone(), t.key.clone()]);
        let blocks = c.certificates.iter().flat_map(|b| [b.cert.clone(), b.key.clone()]);
        vhosts.chain(blocks).flatten().collect()
    };
    named(new).iter().any(|p| old.read_file(p).ok() != new.read_file(p).ok())
}

fn verb(before: bool, after: bool) -> &'static str {
    match (before, after) {
        (false, true) => "added",
        (true, false) => "removed",
        _ => "changed",
    }
}

/// Both maps' entries by key: (old, new).
fn pair<'a, K: Ord + Clone, V>(old: &'a BTreeMap<K, V>, new: &'a BTreeMap<K, V>) -> BTreeMap<K, (Option<&'a V>, Option<&'a V>)> {
    let mut out: BTreeMap<K, (Option<&V>, Option<&V>)> = BTreeMap::new();
    for (k, v) in old {
        out.entry(k.clone()).or_default().0 = Some(v);
    }
    for (k, v) in new {
        out.entry(k.clone()).or_default().1 = Some(v);
    }
    out
}

/// A part of the config as JSON, for comparison.
fn json<T: serde::Serialize>(v: &T) -> serde_json::Value {
    serde_json::to_value(v).unwrap_or_default()
}

/// ANSI colors, when the output is a terminal and color is not turned off.
struct Paint {
    on: bool,
}

impl Paint {
    fn wrap(&self, code: &str, s: &str) -> String {
        if self.on {
            format!("\x1b[{code}m{s}\x1b[0m")
        } else {
            s.to_owned()
        }
    }
    fn red(&self, s: &str) -> String {
        self.wrap("31", s)
    }
    fn green(&self, s: &str) -> String {
        self.wrap("32", s)
    }
    fn yellow(&self, s: &str) -> String {
        self.wrap("33", s)
    }
    fn cyan(&self, s: &str) -> String {
        self.wrap("36", s)
    }
    fn bold(&self, s: &str) -> String {
        self.wrap("1", s)
    }
}

/// A unified diff of two versions of a file, three lines of context.
fn diff(out: &Paint, old_name: &str, new_name: &str, old: &str, new: &str) -> String {
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
    let mut text = format!("{}\n{}\n", out.bold(&format!("--- {old_name}")), out.bold(&format!("+++ {new_name}")));
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
        text.push_str(&out.cyan(&format!("@@ -{},{old_len} +{},{new_len} @@", hunk[0].0 + 1, hunk[0].1 + 1)));
        text.push('\n');
        for &(i, j, tag) in hunk {
            let line = format!("{tag}{}", if tag == '+' { b[j] } else { a[i] }.trim_end_matches('\n'));
            text.push_str(&match tag {
                '+' => out.green(&line),
                '-' => out.red(&line),
                _ => line,
            });
            text.push('\n');
        }
    }
    text
}

/// Every `*.hcl` file below `dir`, sorted; dot files skipped.
fn hcl_files(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else { return Ok(out) };
    for entry in entries {
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

#[cfg(test)]
mod tests {
    use super::*;

    const PLAIN: Paint = Paint { on: false };

    #[test]
    fn unified_diff() {
        let d = diff(&PLAIN, "old/a.hcl", "new/a.hcl", "x {\nb=1\n}\n", "x {\n  b = 1\n}\n");
        assert_eq!(d, "--- old/a.hcl\n+++ new/a.hcl\n@@ -1,3 +1,3 @@\n x {\n-b=1\n+  b = 1\n }\n");
        assert_eq!(diff(&PLAIN, "a", "b", "same\n", "same\n"), "");
    }

    fn cfg(text: &str) -> Config {
        super::super::parse(text).unwrap()
    }

    #[test]
    fn changes_are_marked_reload_or_restart() {
        let old = cfg("listener \"http\" {\n  protocol  = \"http\"\n  addresses = [\"0.0.0.0\"]\n  port      = 80\n}\npool \"web\" {\n  backend \"10.0.0.1:80\" {}\n  backend \"10.0.0.2:80\" {}\n}\nvhost \"*\" {\n  pool = \"web\"\n}\n");
        let new = cfg("listener \"http\" {\n  protocol  = \"http\"\n  addresses = [\"0.0.0.0\"]\n  port      = 80\n}\nlistener \"https\" {\n  protocol  = \"https\"\n  addresses = [\"0.0.0.0\"]\n  port      = 443\n}\npool \"web\" {\n  algorithm = \"random\"\n  backend \"10.0.0.1:80\" { weight = 2 }\n  backend \"10.0.0.3:80\" {}\n}\npool \"api\" {\n  backend \"10.0.0.9:80\" {}\n}\nvhost \"*\" {\n  pool = \"api\"\n}\n");
        let got = changes(&old, &new, false);
        let want = [
            (true, "listener \"https\" added"),
            (false, "pool \"api\" added"),
            (false, "pool \"web\": algorithm"),
            (false, "pool \"web\": backend \"10.0.0.1:80\" weight"),
            (false, "pool \"web\": backend \"10.0.0.2:80\" removed, drains"),
            (false, "pool \"web\": backend \"10.0.0.3:80\" added"),
            (false, "vhosts"),
        ];
        assert_eq!(got, want.iter().map(|(r, w)| (*r, w.to_string())).collect::<Vec<_>>());
        assert_eq!(changes(&old, &old, true), vec![(true, "the node file".to_owned())]);
        assert!(changes(&old, &old, false).is_empty());
    }
}
