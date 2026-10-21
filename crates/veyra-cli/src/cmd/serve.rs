//! `veyra serve`: a local, read-only web viewer for this repository.
//!
//! It shows what a Git host can't: the operation journal and undo position, checkpoints,
//! intents, attestations, signature status, semantic changes and impact.
//!
//! Safety: it binds to 127.0.0.1 only, every API request must carry a per-run token (kept in
//! the URL fragment, so it never appears in requests, logs or Referer headers), requests whose
//! `Host` isn't the loopback address are rejected (DNS rebinding), there are no CORS headers,
//! and only GET is served — the viewer never changes the repository.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};
use veyra_core::diff::NewSide;
use veyra_core::{ByteSlice, FlatTree, ObjectId, Repo, flat};

use crate::cli::ScopeArgs;
use crate::{Ctx, ui};

const INDEX_HTML: &str = include_str!("../serve/index.html");
const APP_JS: &str = include_str!("../serve/app.js");
const APP_CSS: &str = include_str!("../serve/app.css");
const ATTESTATIONS_REF: &str = "refs/notes/veyra/attestations";

fn random_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    // RandomState is seeded from the OS's random source.
    let mut out = String::new();
    for i in 0..2u64 {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(i ^ std::process::id() as u64);
        h.write_u128(
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0),
        );
        out.push_str(&format!("{:016x}", h.finish()));
    }
    out
}

pub fn serve(ctx: &Ctx, port: u16, no_open: bool) -> Result<()> {
    let repo = ctx.repo()?;
    let root = repo.workdir()?.to_owned();
    drop(repo);
    let mut listener = None;
    for p in port..port.saturating_add(20) {
        if let Ok(l) = TcpListener::bind(("127.0.0.1", p)) {
            listener = Some(l);
            break;
        }
    }
    let listener = listener.with_context(|| format!("no free port in {port}–{}", port.saturating_add(19)))?;
    let port = listener.local_addr()?.port();
    let token = random_token();
    let url = format!("http://127.0.0.1:{port}/#token={token}");
    anstream::println!("Veyra viewer for {} on {}", root.display(), ui::bold(&url));
    anstream::println!("{}", ui::dim("Read-only, reachable from this computer only. Press Ctrl+C to stop."));
    if !no_open && !ctx.agent && std::io::IsTerminal::is_terminal(&std::io::stdout()) {
        open_browser(&url);
    }
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let (root, token) = (root.clone(), token.clone());
        std::thread::spawn(move || {
            let _ = handle(stream, &root, &token, port);
        });
    }
    Ok(())
}

fn open_browser(url: &str) {
    let result = if cfg!(windows) {
        std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn()
    } else if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(url).spawn()
    };
    if result.is_err() {
        ui::hint("open the address above in your browser");
    }
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
}

fn decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < bytes.len() => {
                if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("zz"), 16) {
                    out.push(v);
                    i += 2;
                } else {
                    out.push(b'%');
                }
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn read_request(stream: &TcpStream) -> Result<Request> {
    let mut reader = BufReader::new(stream.take(64 * 1024));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or("/").to_owned();
    let mut headers = HashMap::new();
    loop {
        let mut h = String::new();
        if reader.read_line(&mut h)? == 0 || h.trim().is_empty() {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_owned());
        }
    }
    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
    let query = q
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    Ok(Request { method, path: decode(path), query, headers })
}

fn respond(mut stream: &TcpStream, status: &str, content_type: &str, body: &[u8]) -> Result<()> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nX-Frame-Options: DENY\r\n\
         Content-Security-Policy: default-src 'self'; style-src 'self'; script-src 'self'; img-src 'self' data:; frame-ancestors 'none'\r\n\
         Connection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;
    Ok(())
}

fn handle(stream: TcpStream, root: &Path, token: &str, port: u16) -> Result<()> {
    let req = read_request(&stream)?;
    let host_ok =
        req.headers.get("host").is_some_and(|h| *h == format!("127.0.0.1:{port}") || *h == format!("localhost:{port}"));
    if !host_ok {
        return respond(&stream, "421 Misdirected Request", "text/plain", b"wrong host");
    }
    if req.method != "GET" {
        return respond(&stream, "405 Method Not Allowed", "text/plain", b"the viewer is read-only");
    }
    match req.path.as_str() {
        "/" | "/index.html" => return respond(&stream, "200 OK", "text/html; charset=utf-8", INDEX_HTML.as_bytes()),
        "/app.js" => return respond(&stream, "200 OK", "text/javascript; charset=utf-8", APP_JS.as_bytes()),
        "/app.css" => return respond(&stream, "200 OK", "text/css; charset=utf-8", APP_CSS.as_bytes()),
        _ => {}
    }
    let Some(api) = req.path.strip_prefix("/api/") else {
        return respond(&stream, "404 Not Found", "text/plain", b"not found");
    };
    if req.headers.get("x-veyra-token").map(String::as_str) != Some(token) {
        return respond(
            &stream,
            "403 Forbidden",
            "application/json",
            br#"{"error":"missing or wrong token; open the address veyra serve printed"}"#,
        );
    }
    let result = Repo::open(root).and_then(|repo| route(&repo, api, &req.query));
    match result {
        Ok(Reply::Json(v)) => respond(&stream, "200 OK", "application/json", serde_json::to_string(&v)?.as_bytes()),
        Ok(Reply::Text(t)) => respond(&stream, "200 OK", "text/plain; charset=utf-8", t.as_bytes()),
        Err(e) => {
            let body = json!({ "error": format!("{e:#}") });
            respond(&stream, "500 Internal Server Error", "application/json", body.to_string().as_bytes())
        }
    }
}

enum Reply {
    Json(Value),
    Text(String),
}

fn route(repo: &Repo, api: &str, query: &HashMap<String, String>) -> Result<Reply> {
    let param = |k: &str| query.get(k).map(String::as_str);
    Ok(match api.split_once('/').unwrap_or((api, "")) {
        ("summary", _) => Reply::Json(summary(repo)?),
        ("log", _) => {
            let skip = param("skip").and_then(|s| s.parse().ok()).unwrap_or(0);
            let limit = param("limit").and_then(|s| s.parse().ok()).unwrap_or(100usize).min(500);
            Reply::Json(log(repo, param("rev"), skip, limit)?)
        }
        ("commit", id) => Reply::Json(commit(repo, id)?),
        ("diff", id) => Reply::Text(diff(repo, id, param("path").unwrap_or_default())?),
        ("journal", _) => Reply::Json(journal(repo)?),
        ("checkpoints", _) => Reply::Json(checkpoints(repo)?),
        ("intents", _) => Reply::Json(intents(repo)?),
        ("impact", _) => Reply::Json(impact(repo, param("scope").unwrap_or("uncommitted"))?),
        ("branches", _) => Reply::Json(branches(repo)?),
        _ => bail!("unknown endpoint /api/{api}"),
    })
}

// ------------------------------------------------------------------------------------------
// data

fn decorations(repo: &Repo) -> Result<BTreeMap<ObjectId, Vec<Value>>> {
    let mut out: BTreeMap<ObjectId, Vec<Value>> = BTreeMap::new();
    let head = repo.head_state()?;
    let head_branch = head.branch_short().map(str::to_owned);
    if head_branch.is_none()
        && let Some(c) = head.commit()
    {
        out.entry(c).or_default().push(json!({"kind": "head", "name": "HEAD"}));
    }
    for (name, id) in repo.all_refs()? {
        let Ok(commit) = repo.peel_to_commit(id) else { continue };
        let (kind, short) = if let Some(b) = name.strip_prefix("refs/heads/") {
            (if head_branch.as_deref() == Some(b) { "current" } else { "branch" }, b)
        } else if let Some(t) = name.strip_prefix("refs/tags/") {
            ("tag", t)
        } else if let Some(r) = name.strip_prefix("refs/remotes/") {
            if r.ends_with("/HEAD") {
                continue;
            }
            ("remote", r)
        } else {
            continue;
        };
        out.entry(commit).or_default().push(json!({"kind": kind, "name": short}));
    }
    Ok(out)
}

fn noted(repo: &Repo, notes_ref: &str) -> HashSet<ObjectId> {
    repo.notes(notes_ref).map(|n| n.into_iter().map(|(id, _)| id).collect()).unwrap_or_default()
}

fn summary(repo: &Repo) -> Result<Value> {
    let root = repo.workdir()?.to_owned();
    let head = repo.head_state()?;
    let head_commit = match head.commit() {
        Some(id) => {
            let c = repo.commit_info(id)?;
            json!({"id": id.to_string(), "short": veyra_core::short(&id), "summary": c.summary()})
        }
        None => Value::Null,
    };
    let status = repo.status()?;
    let log = veyra_ops::OpLog::open(repo)?;
    let ops = log.operations()?;
    let (undo, redo) = veyra_ops::targets(&ops);
    let op_json = |o: Option<&veyra_ops::Operation>| {
        o.map(|o| json!({"id": o.id, "short": o.short_id(), "kind": o.kind, "summary": o.summary}))
            .unwrap_or(Value::Null)
    };
    let in_progress = if repo.merge_head()?.is_some() {
        Some("merge")
    } else if repo.rebase_in_progress()?.is_some() {
        Some("rebase")
    } else if repo.pick_in_progress()?.is_some() {
        Some("cherry-pick or revert")
    } else if repo.bisect_state()?.is_some() {
        Some("bisect")
    } else {
        None
    };
    Ok(json!({
        "name": root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default(),
        "path": root.display().to_string(),
        "branch": head.branch_short(),
        "head": head_commit,
        "in_progress": in_progress,
        "status": {
            "staged": status.staged.len(),
            "modified": status.worktree.modified.len() + status.worktree.deleted.len(),
            "untracked": status.worktree.untracked.len(),
            "conflicted": status.worktree.conflicted.len(),
        },
        "undo": op_json(undo),
        "redo": op_json(redo),
        "operations": ops.len(),
        "signing": repo.config_get("user.signingKey").is_some(),
        "ai": crate::cmd::ai::settings(repo).describe(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

/// The trailers of a commit message, and the ones Veyra gives meaning to.
struct Trailers {
    all: Vec<(String, String)>,
    assisted: Vec<String>,
    intents: Vec<String>,
    structural: Vec<String>,
}

fn trailer_lists(message: &str) -> Trailers {
    let all = veyra_core::commit::parse_trailers(message);
    let pick =
        |key: &str| all.iter().filter(|(k, _)| k.eq_ignore_ascii_case(key)).map(|(_, v)| v.clone()).collect::<Vec<_>>();
    Trailers { assisted: pick("Assisted-by"), intents: pick("Intent"), structural: pick("Veyra-Structural-Merge"), all }
}

fn log(repo: &Repo, rev: Option<&str>, skip: usize, limit: usize) -> Result<Value> {
    let tips: Vec<ObjectId> = match rev {
        Some(r) if !r.is_empty() => vec![repo.rev_parse_commit(r)?],
        _ => repo.head_commit_id()?.into_iter().collect(),
    };
    let decor = decorations(repo)?;
    let attested = noted(repo, ATTESTATIONS_REF);
    let notes = noted(repo, veyra_core::notes::DEFAULT_NOTES_REF);
    let commits = repo.log(&tips, Some(skip + limit + 1))?;
    let more = commits.len() > skip + limit;
    let entries: Vec<Value> = commits
        .iter()
        .skip(skip)
        .take(limit)
        .map(|c| {
            let Trailers { assisted, intents, structural, .. } = trailer_lists(&c.message);
            json!({
                "id": c.id.to_string(),
                "short": veyra_core::short(&c.id),
                "summary": c.summary(),
                "author": c.author_name,
                "time": c.author_time,
                "parents": c.parents.iter().map(|p| p.to_string()).collect::<Vec<_>>(),
                "refs": decor.get(&c.id).cloned().unwrap_or_default(),
                "assisted": assisted,
                "intents": intents,
                "structural": structural,
                "attested": attested.contains(&c.id),
                "noted": notes.contains(&c.id),
            })
        })
        .collect();
    Ok(json!({ "commits": entries, "more": more }))
}

fn commit(repo: &Repo, spec: &str) -> Result<Value> {
    let id = repo.rev_parse_commit(spec)?;
    let c = repo.commit_info(id)?;
    let Trailers { all: trailers, assisted, intents: intent_ids, structural } = trailer_lists(&c.message);
    let parent = match c.parents.first() {
        Some(p) => repo.flat_from_commit(*p)?,
        None => FlatTree::new(),
    };
    let changes = flat::diff(&parent, &repo.flat_from_tree(c.tree)?);
    let diffs = repo.diff_changes(&changes, NewSide::Objects, 3)?;
    let semantic = repo.semantic_diff_changes(&changes)?;
    let files: Vec<Value> = diffs
        .iter()
        .zip(&semantic)
        .zip(&changes)
        .map(|((d, s), ch)| {
            let symbols: Vec<Value> = s
                .diff
                .as_ref()
                .map(|sd| {
                    sd.changes
                        .iter()
                        .map(|x| json!({"change": x.change.label(), "kind": x.kind.label(), "name": x.name, "old_name": x.old_name, "line": x.new_line.or(x.old_line)}))
                        .collect()
                })
                .unwrap_or_default();
            json!({
                "path": d.path.to_string(),
                "status": ch.status_letter().to_string(),
                "added": d.added,
                "removed": d.removed,
                "binary": d.binary,
                "language": s.diff.as_ref().map(|sd| sd.lang.name()),
                "formatting_only": s.diff.as_ref().is_some_and(|sd| sd.formatting_only()),
                "symbols": symbols,
            })
        })
        .collect();
    let signature = match repo.verify_commit(id)?.outcome {
        None => json!({"status": "unsigned"}),
        Some(o) => {
            json!({"status": if o.valid { "good" } else { "bad" }, "format": o.format, "signer": o.signer.or(o.key), "trust": o.trust})
        }
    };
    let attestations: Vec<Value> = repo
        .attestations(id)?
        .iter()
        .map(|a| {
            let check = repo.check_attestation(a).ok();
            json!({
                "type": a.predicate_type(),
                "predicate": a.statement.get("predicate"),
                "signed": a.signature.is_some(),
                "signer": a.signature.as_ref().map(|s| s.signer.clone()),
                "valid": check.map(|c| format!("{c:?}")),
            })
        })
        .collect();
    let root = repo.workdir()?.to_owned();
    let all_intents = veyra_intent::load_all(&root).unwrap_or_default();
    let intents: Vec<Value> = intent_ids
        .iter()
        .map(|iid| match all_intents.iter().find(|(_, i)| i.id == *iid) {
            Some((_, i)) => {
                json!({"id": i.id, "title": i.title, "status": i.status.to_string(), "objective": i.objective})
            }
            None => json!({"id": iid, "title": null}),
        })
        .collect();
    let decor = decorations(repo)?;
    Ok(json!({
        "id": id.to_string(),
        "short": veyra_core::short(&id),
        "summary": c.summary(),
        "message": c.message,
        "author": c.author_name,
        "email": c.author_email,
        "time": c.author_time,
        "committer": c.committer_name,
        "committer_time": c.committer_time,
        "parents": c.parents.iter().map(|p| json!({"id": p.to_string(), "short": veyra_core::short(p)})).collect::<Vec<_>>(),
        "refs": decor.get(&id).cloned().unwrap_or_default(),
        "trailers": trailers,
        "assisted": assisted,
        "structural": structural,
        "intents": intents,
        "signature": signature,
        "attestations": attestations,
        "note": repo.note(veyra_core::notes::DEFAULT_NOTES_REF, id)?,
        "files": files,
    }))
}

fn diff(repo: &Repo, spec: &str, path: &str) -> Result<String> {
    let id = repo.rev_parse_commit(spec)?;
    let c = repo.commit_info(id)?;
    let parent = match c.parents.first() {
        Some(p) => repo.flat_from_commit(*p)?,
        None => FlatTree::new(),
    };
    let changes: Vec<_> = flat::diff(&parent, &repo.flat_from_tree(c.tree)?)
        .into_iter()
        .filter(|ch| ch.path.to_str_lossy() == path)
        .collect();
    let diffs = repo.diff_changes(&changes, NewSide::Objects, 3)?;
    Ok(diffs.iter().map(|d| veyra_core::diff::render(d, None)).collect())
}

fn journal(repo: &Repo) -> Result<Value> {
    let ops = veyra_ops::OpLog::open(repo)?.operations()?;
    let (undo, redo) = veyra_ops::targets(&ops);
    // Which operations are currently undone (same bookkeeping as `targets`).
    let mut undone: BTreeSet<&str> = BTreeSet::new();
    for op in &ops {
        match (op.kind.as_str(), op.target.as_deref()) {
            ("undo", Some(t)) => {
                undone.insert(t);
            }
            ("redo", Some(t)) => {
                undone.remove(t);
            }
            _ => {}
        }
    }
    let entries: Vec<Value> = ops
        .iter()
        .rev()
        .map(|o| {
            json!({
                "id": o.id,
                "short": o.short_id(),
                "time": o.time,
                "kind": o.kind,
                "summary": o.summary,
                "command": o.command,
                "target": o.target,
                "undoable": o.is_undoable(),
                "undone": undone.contains(o.id.as_str()),
                "files": o.worktree_changes,
                "snapshot": o.before.worktree.is_some() || o.after.worktree.is_some(),
            })
        })
        .collect();
    Ok(json!({
        "operations": entries,
        "undo": undo.map(|o| o.id.clone()),
        "redo": redo.map(|o| o.id.clone()),
    }))
}

fn checkpoints(repo: &Repo) -> Result<Value> {
    let mut out = Vec::new();
    let mut last: Option<String> = None;
    for op in veyra_ops::OpLog::open(repo)?.operations()? {
        let tree = if op.kind == "checkpoint" { op.after.worktree.clone() } else { op.before.worktree.clone() };
        let Some(tree) = tree else { continue };
        if last.as_ref() == Some(&tree) {
            continue;
        }
        last = Some(tree.clone());
        let source = if op.kind == "checkpoint" { op.summary.clone() } else { format!("before {}", op.summary) };
        out.push(json!({"id": op.id, "short": op.short_id(), "time": op.time, "source": source, "automatic": op.kind != "checkpoint", "files": op.worktree_changes}));
    }
    out.reverse();
    Ok(json!(out))
}

fn intents(repo: &Repo) -> Result<Value> {
    let root = repo.workdir()?.to_owned();
    let source = crate::cmd::tools::WorktreeSource::new(repo)?;
    let mut out = Vec::new();
    for (path, i) in veyra_intent::load_all(&root)? {
        let violations: Vec<String> =
            i.verify(&source).map(|v| v.into_iter().map(|x| x.detail).collect()).unwrap_or_default();
        let guards: Vec<Value> = i
            .guards
            .iter()
            .map(|g| {
                let check = g
                    .must_contain
                    .as_ref()
                    .map(|c| format!("must contain {c}"))
                    .or(g.must_match.as_ref().map(|m| format!("must match /{m}/")))
                    .or(g.must_not_contain.as_ref().map(|m| format!("must not contain {m}")))
                    .unwrap_or_default();
                json!({"paths": g.paths, "check": check, "message": g.message})
            })
            .collect();
        out.push(json!({
            "id": i.id, "title": i.title, "kind": i.kind, "status": i.status.to_string(), "created": i.created,
            "objective": i.objective, "problem": i.problem, "solution": i.solution, "constraints": i.constraints,
            "related": i.related, "guards": guards, "violations": violations,
            "file": path.strip_prefix(&root).unwrap_or(&path).display().to_string(),
        }));
    }
    Ok(json!(out))
}

fn impact(repo: &Repo, scope: &str) -> Result<Value> {
    let args = match scope {
        "staged" => ScopeArgs { staged: true, ..Default::default() },
        "branch" => ScopeArgs { branch: Some(String::new()), ..Default::default() },
        _ => ScopeArgs::default(),
    };
    let (old, new, what) = super::analysis::scope(repo, &args)?;
    let report = repo.impact(&old, &new, &veyra_core::impact::ImpactOptions { history_limit: 300 })?;
    Ok(json!({"scope": what, "report": report}))
}

fn branches(repo: &Repo) -> Result<Value> {
    let current = repo.head_state()?.branch_short().map(str::to_owned);
    let nodes: Vec<Value> = repo
        .branch_tree()?
        .iter()
        .map(|b| {
            let c = repo.commit_info(b.tip).ok();
            json!({
                "name": b.name, "parent": b.parent, "ahead": b.ahead, "behind": b.behind,
                "tip": veyra_core::short(&b.tip), "summary": c.as_ref().map(|c| c.summary().to_owned()),
                "time": c.map(|c| c.committer_time), "current": current.as_deref() == Some(b.name.as_str()),
            })
        })
        .collect();
    Ok(json!(nodes))
}
