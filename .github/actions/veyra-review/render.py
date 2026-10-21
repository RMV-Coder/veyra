#!/usr/bin/env python3
"""Render Veyra's JSON analyses of a pull request as one Markdown review.

    render.py <dir> [--max-chars N] [--summary FILE] [--github-output FILE] > comment.md

<dir> holds what collect.sh wrote: meta.txt, impact.json, analyze.json, semantic.json,
commits.json and verify.json, each with a matching .err file. A missing or unparsable output is
reported in its section; the rest of the review still renders.

The first line is a hidden marker the action uses to find and update its own PR comment.
Standard library only (Python 3.8+), so it runs on any GitHub-hosted runner.
"""

import argparse
import json
import os
import re
import sys

MARKER = "<!-- veyra-review -->"
STRUCTURAL_TRAILER = "Veyra-Structural-Merge"
# GitHub rejects comments over 65536 characters; a step summary can hold 1 MiB.
DEFAULT_MAX_CHARS = 60000
SUMMARY_MAX_CHARS = 900000

SEVERITIES = ("high", "medium", "low")
RISK_ICON = {"high": "🔴", "medium": "🟠", "low": "🟢"}
SEVERITY_ICON = {"high": "🔴", "medium": "🟠", "low": "⚪"}
CONFIDENCE_ORDER = {"high": 0, "medium": 1, "low": 2}
CHANGE_MARK = {
    "added": "+",
    "removed": "−",
    "modified": "~",
    "signature changed": "~",
    "reformatted": "=",
    "renamed": "→",
}

MAX_FINDINGS = 50
MAX_DEPENDENTS = 40
MAX_EVIDENCE = 3
MAX_TESTS = 50
MAX_SYMBOL_FILES = 40
MAX_SYMBOLS_PER_FILE = 25
MAX_COMMITS_LISTED = 30
MAX_CO_CHANGES = 10


# ------------------------------------------------------------------------------------------
# inputs


class Output:
    """One analysis' parsed JSON values, or why there are none."""

    def __init__(self, values, error):
        self.values = values
        self.error = error

    @property
    def ok(self):
        return self.error is None

    def first(self):
        return self.values[0] if self.values else None


def parse_stream(text):
    """Every top-level JSON value in `text` (commands appended one after another)."""
    decoder = json.JSONDecoder()
    values, i = [], 0
    while True:
        while i < len(text) and text[i].isspace():
            i += 1
        if i >= len(text):
            return values
        value, i = decoder.raw_decode(text, i)
        values.append(value)


def read_text(path):
    try:
        with open(path, encoding="utf-8", errors="replace") as f:
            return f.read()
    except OSError:
        return None


def load(directory, name):
    text = read_text(os.path.join(directory, name + ".json"))
    err = (read_text(os.path.join(directory, name + ".err")) or "").strip()
    if text is None:
        return Output([], err or "not run")
    try:
        values = parse_stream(text)
    except ValueError as e:
        return Output([], err or "unparsable output: %s" % e)
    if not values and err:
        return Output([], err)
    return Output(values, None)


def load_meta(directory):
    meta = {}
    for line in (read_text(os.path.join(directory, "meta.txt")) or "").splitlines():
        key, sep, value = line.partition("=")
        if sep:
            meta[key.strip()] = value.strip()
    return meta


# ------------------------------------------------------------------------------------------
# escaping: paths, commit messages and code excerpts come from the pull request


def one_line(s):
    return " ".join(str(s).split())


def neutralize(s):
    """No HTML (comment markers, tags) and no @mentions; Markdown is kept."""
    s = s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")
    return s.replace("@", "@​")


def prose(s, table=False):
    """Text Veyra wrote (reasons, evidence, advice): Markdown is kept, HTML and mentions aren't."""
    s = neutralize(one_line(s))
    return s.replace("|", "\\|") if table else s


def text(s, table=False):
    """Untrusted free text (commit summaries): rendered literally."""
    s = re.sub(r"([\\`*_\[\]~#!])", r"\\\1", one_line(s))
    return prose(s, table)


def code(s, table=False):
    """An inline code span that survives backticks in `s` (and pipes, inside a table)."""
    s = one_line(s)
    longest = max((len(m) for m in re.findall(r"`+", s)), default=0)
    fence = "`" * (longest + 1)
    pad = " " if s.startswith("`") or s.endswith("`") else ""
    if table:
        s = s.replace("|", "\\|")
    return "%s%s%s%s%s" % (fence, pad, s, pad, fence)


def plural(n, word, words=None):
    return "%d %s" % (n, word if n == 1 else (words or word + "s"))


def error_note(what, output):
    lines = [l for l in output.error.splitlines() if l.strip()][-5:]
    detail = "\n".join(lines) or "no output"
    return "_%s is unavailable:_\n\n```text\n%s\n```\n" % (what, detail.replace("```", "'''"))


# ------------------------------------------------------------------------------------------
# sections


def header(meta, impact):
    risk = (impact or {}).get("risk") or {}
    level = risk.get("level")
    if level in RISK_ICON:
        title = "## %s Veyra review: %s risk" % (RISK_ICON[level], level)
    else:
        title = "## Veyra review"
    files = (impact or {}).get("changed_files") or []
    facts = []
    base, head = meta.get("base"), meta.get("head")
    if base and head:
        facts.append("%s → %s" % (code(base), code(head[:7])))
    if impact is not None:
        added = sum(f.get("added_lines", 0) for f in files)
        removed = sum(f.get("removed_lines", 0) for f in files)
        facts.append("%s (+%d −%d)" % (plural(len(files), "file"), added, removed))
    if meta.get("commits", "").isdigit():
        facts.append(plural(int(meta["commits"]), "commit"))
    if "score" in risk:
        facts.append("risk score %s" % risk["score"])
    out = [MARKER, title, ""]
    if facts:
        out += [" · ".join(facts), ""]
    reasons = risk.get("reasons") or []
    if reasons:
        out += ["**Why:**"] + ["- %s" % prose(r) for r in reasons] + [""]
    return "\n".join(out)


def overview(impact, findings, semantic, commits, verify):
    rows = []
    if findings is not None:
        counts = [(s, sum(1 for f in findings if f.get("severity") == s)) for s in SEVERITIES]
        result = ", ".join("%s %d %s" % (SEVERITY_ICON[s], n, s) for s, n in counts if n) or "none"
        rows.append(("Analyzer findings", result))
    if impact is not None:
        tests = impact.get("tests") or []
        rows.append(("Tests to run", str(len(tests)) if tests else "none found"))
        deps = impact.get("dependents") or []
        high = sum(1 for d in deps if d.get("confidence") == "high")
        affected = "%s (%d high confidence)" % (plural(len(deps), "file"), high)
        rows.append(("May be affected", affected if deps else "none found"))
    if semantic is not None:
        changed = [f for f in semantic if f.get("supported") and f.get("changes")]
        n = sum(len(f["changes"]) for f in changed)
        rows.append(("Changed symbols", "%d in %s" % (n, plural(len(changed), "file"))))
    if verify is not None:
        s = signature_counts(verify)
        # "unverified" only when it happens: most repositories have good, unsigned and bad only.
        shown = ["good"] + (["unverified"] if s["unverified"] else []) + ["unsigned", "bad"]
        cell = " · ".join("%d %s" % (s[k], k) for k in shown)
        if s["bad"]:
            cell = "❌ " + cell
        rows.append(("Signatures", cell))
    if commits is not None:
        merged = [c for c in commits if structural_files(c)]
        if merged:
            rows.append(("Structural merges", plural(len(merged), "commit")))
    if not rows:
        return ""
    lines = ["| Check | Result |", "|---|---|"]
    lines += ["| %s | %s |" % row for row in rows]
    return "\n".join(lines) + "\n"


def findings_section(output):
    out = ["### Analyzer findings", ""]
    if not output.ok:
        return "\n".join(out) + "\n" + error_note("`veyra analyze`", output)
    findings = output.first() or []
    if not findings:
        return "\n".join(out + ["No migration or infrastructure risks found.", ""])
    order = {s: i for i, s in enumerate(SEVERITIES)}
    findings = sorted(findings, key=lambda f: (order.get(f.get("severity"), 9), f.get("path", ""), f.get("line", 0)))
    out += ["| Severity | Location | Finding | Advice |", "|---|---|---|---|"]
    for f in findings[:MAX_FINDINGS]:
        sev = f.get("severity", "?")
        loc = f.get("path", "")
        if f.get("line"):
            loc = "%s:%d" % (loc, f["line"])
        finding = prose(f.get("message", ""), table=True)
        if f.get("excerpt"):
            finding += "<br>%s" % code(f["excerpt"], table=True)
        if f.get("rule"):
            finding += " <sub>%s</sub>" % code(f["rule"], table=True)
        out.append(
            "| %s %s | %s | %s | %s |"
            % (SEVERITY_ICON.get(sev, ""), sev, code(loc, table=True), finding, prose(f.get("advice", ""), table=True))
        )
    if len(findings) > MAX_FINDINGS:
        out.append("\n…and %d more (`veyra analyze --branch <base>` lists all)." % (len(findings) - MAX_FINDINGS))
    out += ["", "<sub>Advisory and pattern-based. Silence a rule with `disable = [\"<rule>\"]` in `.veyra/analyzers.toml`.</sub>", ""]
    return "\n".join(out)


def tests_section(impact):
    tests = impact.get("tests") or []
    out = ["### Tests to run", ""]
    if not tests:
        return "\n".join(out + ["None found for this change.", ""])
    out += ["- %s — %s" % (code(t.get("path", "")), prose(t.get("reason", ""))) for t in tests[:MAX_TESTS]]
    if len(tests) > MAX_TESTS:
        out.append("- …and %d more" % (len(tests) - MAX_TESTS))
    return "\n".join(out + [""])


def dependents_section(impact):
    deps = impact.get("dependents") or []
    if not deps:
        return ""
    deps = sorted(deps, key=lambda d: (CONFIDENCE_ORDER.get(d.get("confidence"), 9), d.get("path", "")))
    counts = ", ".join(
        "%d %s" % (n, c) for c in CONFIDENCE_ORDER for n in [sum(1 for d in deps if d.get("confidence") == c)] if n
    )
    summary = "May be affected: %s (%s confidence)" % (plural(len(deps), "file"), counts)
    out = ["<details%s><summary><b>%s</b></summary>" % (" open" if len(deps) <= 10 else "", summary), ""]
    for d in deps[:MAX_DEPENDENTS]:
        conf = d.get("confidence", "?")
        test = " (test)" if d.get("is_test") else ""
        out.append("- %s **%s** %s%s" % (SEVERITY_ICON.get(conf, ""), conf, code(d.get("path", "")), test))
        evidence = d.get("evidence") or []
        out += ["  - %s" % prose(e) for e in evidence[:MAX_EVIDENCE]]
        if len(evidence) > MAX_EVIDENCE:
            out.append("  - …and %d more" % (len(evidence) - MAX_EVIDENCE))
    if len(deps) > MAX_DEPENDENTS:
        out.append("\n…and %d more (`veyra impact --branch <base> --json` lists all)." % (len(deps) - MAX_DEPENDENTS))
    return "\n".join(out + ["", "</details>", ""])


def symbol_line(c):
    change = c.get("change", "")
    name = c.get("name", "")
    if change == "renamed" and c.get("old_name"):
        name = "%s → %s" % (c["old_name"], name)
    line = c.get("new_line") or c.get("old_line")
    s = "- `%s` %s %s" % (CHANGE_MARK.get(change, "·"), c.get("kind", ""), code(name))
    s += " — %s" % change
    if line:
        s += " (line %d)" % line
    if change == "signature changed" and c.get("old_signature") and c.get("new_signature"):
        s += ": %s → %s" % (code(c["old_signature"]), code(c["new_signature"]))
    return s


def symbols_section(semantic, impact):
    title = "### Changed symbols"
    if not semantic.ok:
        # `impact` lists symbols too, without lines and signatures.
        files = [f for f in (impact or {}).get("changed_files") or [] if f.get("symbols")]
        if not files:
            return title + "\n\n" + error_note("`veyra diff --semantic`", semantic)
        semantic_files = [
            {"path": f["path"], "supported": True, "language": f.get("language"), "changes": f["symbols"]} for f in files
        ]
    else:
        semantic_files = semantic.first() or []
    changed = [f for f in semantic_files if f.get("supported") and f.get("changes")]
    unsupported = [f for f in semantic_files if not f.get("supported")]
    n = sum(len(f["changes"]) for f in changed)
    if not changed:
        out = [title, "", "No functions, methods or types changed."]
    else:
        summary = "Changed symbols: %d in %s" % (n, plural(len(changed), "file"))
        out = ["<details%s><summary><b>%s</b></summary>" % (" open" if n <= 30 else "", summary), ""]
        for f in changed[:MAX_SYMBOL_FILES]:
            notes = [f["language"]] if f.get("language") else []
            if f.get("formatting_only"):
                notes.append("formatting only")
            out.append("**%s**%s" % (code(f.get("path", "")), " · %s" % ", ".join(notes) if notes else ""))
            out.append("")
            if f.get("parse_errors"):
                out.append("- ⚠️ syntax errors in one version; results are best-effort")
            out += [symbol_line(c) for c in f["changes"][:MAX_SYMBOLS_PER_FILE]]
            if len(f["changes"]) > MAX_SYMBOLS_PER_FILE:
                out.append("- …and %d more" % (len(f["changes"]) - MAX_SYMBOLS_PER_FILE))
            out.append("")
        if len(changed) > MAX_SYMBOL_FILES:
            out.append("…and %s more." % plural(len(changed) - MAX_SYMBOL_FILES, "file"))
        out += ["</details>"]
    if unsupported:
        names = ", ".join(code(f.get("path", "")) for f in unsupported[:10])
        more = " and %d more" % (len(unsupported) - 10) if len(unsupported) > 10 else ""
        out += ["", "<sub>Without syntax support (line diff only): %s%s.</sub>" % (names, more)]
    return "\n".join(out + [""])


def co_changes_section(impact):
    co = impact.get("co_changes") or []
    if not co:
        return ""
    out = ["### Usually changed together (not in this PR)", ""]
    for c in co[:MAX_CO_CHANGES]:
        out.append("- %s — in %d of %d commits" % (code(c.get("path", "")), c.get("together", 0), c.get("of", 0)))
    return "\n".join(out + [""])


def signature_state(v):
    """good, unverified (a sound signature by a key the runner doesn't trust), bad or unsigned."""
    if not v.get("signed"):
        return "unsigned"
    if v.get("valid"):
        return "good"
    return "unverified" if v.get("status") == "good" else "bad"


def signature_counts(verify):
    counts = {"good": 0, "unverified": 0, "unsigned": 0, "bad": 0}
    for v in verify:
        counts[signature_state(v)] += 1
    return counts


def signature_cell(v):
    if v is None:
        return "?"
    state = signature_state(v)
    if state == "unsigned":
        return "unsigned"
    who = ", ".join(x for x in (v.get("format"), v.get("signer")) if x)
    detail = " (%s)" % text(who, table=True) if who else ""
    if state == "good":
        return "✅ good%s" % detail
    if state == "unverified":
        return "⚠️ unverified key%s, trust %s" % (detail, text(v.get("trust") or "unknown", table=True))
    return "❌ %s%s" % (text(v.get("status") or "bad", table=True), detail)


def trailers(commit):
    return [(t.get("key", ""), t.get("value", "")) for t in commit.get("trailers") or []]


def commit_list(output):
    """`veyra log --json` prints an array per call; collect.sh makes one call per commit."""
    return [c for v in output.values for c in (v if isinstance(v, list) else [v]) if isinstance(c, dict)]


def structural_files(commit):
    return [v for k, v in trailers(commit) if k.lower() == STRUCTURAL_TRAILER.lower()]


def commits_section(commits_out, verify_out, meta):
    out = ["### Commits", ""]
    if not commits_out.ok and not verify_out.ok:
        return "\n".join(out) + "\n" + error_note("Commit information", commits_out)
    commits = commit_list(commits_out)
    by_id = {v.get("object"): v for v in verify_out.values if isinstance(v, dict)}
    if not commits:
        commits = [{"id": oid, "summary": v.get("label", "")} for oid, v in by_id.items()]
    if not commits:
        return "\n".join(out + ["No commits between the base and the head.", ""])

    merged = [(c, structural_files(c)) for c in commits if structural_files(c)]
    if merged:
        out += ["**Merged structurally by Veyra** (conflicts resolved per function; worth a second look):", ""]
        for c, files in merged:
            out.append("- %s: %s" % (code(c.get("id", "")[:7]), ", ".join(code(f) for f in files)))
        out.append("")

    def flagged(c):
        return signature_state(by_id.get(c.get("id"), {})) == "bad" or structural_files(c)

    listed = commits if len(commits) <= MAX_COMMITS_LISTED else [c for c in commits if flagged(c)]
    if len(listed) < len(commits):
        out += ["Showing the %s with a bad signature or a structural merge." % plural(len(listed), "commit"), ""]
    if listed:
        out += ["| Commit | Summary | Signature | Trailers |", "|---|---|---|---|"]
        for c in listed:
            cells = (
                code(c.get("id", "")[:7], table=True),
                text(c.get("summary", ""), table=True),
                signature_cell(by_id.get(c.get("id"))),
                "<br>".join(text("%s: %s" % kv, table=True) for kv in trailers(c)),
            )
            out.append("| %s | %s | %s | %s |" % cells)
    total = int(meta["commits"]) if meta.get("commits", "").isdigit() else len(commits)
    if total > len(commits):
        out.append("\nOnly the newest %d of %d commits were checked." % (len(commits), total))
    if not verify_out.ok:
        out += ["", error_note("Signature verification", verify_out)]
    return "\n".join(out + [""])


def footer(impact, meta):
    out = []
    limitations = (impact or {}).get("limitations") or []
    if limitations:
        out += ["<details><summary>Limitations of this analysis</summary>", ""]
        out += ["- %s" % prose(l) for l in limitations]
        out += ["", "</details>", ""]
    version = meta.get("veyra") or "veyra"
    out.append("<sub>Generated by %s. Deterministic: no AI involved. Reproduce locally with `veyra impact --branch <base>`.</sub>" % neutralize(version))
    return "\n".join(out) + "\n"


# ------------------------------------------------------------------------------------------
# assembly


def render(directory, max_chars=DEFAULT_MAX_CHARS):
    """The review Markdown and the action outputs for the analyses in `directory`."""
    meta = load_meta(directory)
    impact_out = load(directory, "impact")
    analyze_out = load(directory, "analyze")
    semantic_out = load(directory, "semantic")
    commits_out = load(directory, "commits")
    verify_out = load(directory, "verify")

    impact = impact_out.first() if impact_out.ok else None
    findings = (analyze_out.first() or []) if analyze_out.ok else None
    semantic = (semantic_out.first() or []) if semantic_out.ok else None
    commits = commit_list(commits_out) if commits_out.ok else None
    verify = [v for v in verify_out.values if isinstance(v, dict)] if verify_out.ok else None

    if impact is not None and not impact.get("changed_files"):
        body = [header(meta, impact), "No changes between the base and the head.\n", footer(None, meta)]
        return finish(body, [], max_chars), outputs(impact, findings, verify)

    # (section, can be dropped to fit GitHub's comment size limit)
    sections = [(header(meta, impact), False), (overview(impact, findings, semantic, commits, verify), False)]
    sections.append((findings_section(analyze_out), False))
    if impact is not None:
        sections += [(tests_section(impact), True), (dependents_section(impact), True)]
    else:
        sections.append(("### Impact\n\n" + error_note("`veyra impact`", impact_out), False))
    sections.append((symbols_section(semantic_out, impact), True))
    sections.append((commits_section(commits_out, verify_out, meta), True))
    if impact is not None:
        sections.append((co_changes_section(impact), True))
    sections.append((footer(impact, meta), False))
    droppable = [i for i, (_, can_drop) in enumerate(sections) if can_drop]
    return finish([s for s, _ in sections], droppable, max_chars), outputs(impact, findings, verify)


def finish(sections, droppable, max_chars):
    """Join the sections, replacing the largest droppable ones with a note until the text fits."""
    sections = list(sections)
    droppable = [i for i in droppable if sections[i]]

    def joined():
        return "\n".join(s for s in sections if s)

    while len(joined()) > max_chars and droppable:
        i = max(droppable, key=lambda j: len(sections[j]))
        droppable.remove(i)
        title = sections[i].lstrip().splitlines()[0]
        title = re.sub(r"<[^>]+>", "", title).lstrip("# ").strip()
        sections[i] = "_%s: omitted to fit GitHub's comment size limit; the job summary has all of it._\n" % title
    md = joined()
    if len(md) > max_chars:
        md = md[: max_chars - 80] + "\n\n_…truncated to fit GitHub's comment size limit._\n"
    return md


def outputs(impact, findings, verify):
    risk = (impact or {}).get("risk") or {}
    out = {"risk": risk.get("level", ""), "risk-score": str(risk.get("score", ""))}
    findings = findings or []
    out["findings"] = str(len(findings))
    for s in SEVERITIES:
        out["%s-findings" % s] = str(sum(1 for f in findings if f.get("severity") == s))
    out["tests"] = str(len((impact or {}).get("tests") or []))
    counts = signature_counts(verify or [])
    out["unsigned-commits"] = str(counts["unsigned"])
    out["unverified-signatures"] = str(counts["unverified"])
    out["bad-signatures"] = str(counts["bad"])
    return out


def main(argv=None):
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("dir", help="directory written by collect.sh")
    p.add_argument("--max-chars", type=int, default=DEFAULT_MAX_CHARS)
    p.add_argument("--summary", help="also write the review without the comment size limit to this file")
    p.add_argument("--github-output", help="append the action outputs (key=value) to this file")
    args = p.parse_args(argv)
    md, outs = render(args.dir, args.max_chars)
    if args.summary:
        with open(args.summary, "w", encoding="utf-8") as f:
            f.write(render(args.dir, SUMMARY_MAX_CHARS)[0])
    if args.github_output:
        with open(args.github_output, "a", encoding="utf-8") as f:
            for k, v in outs.items():
                f.write("%s=%s\n" % (k, v))
    sys.stdout.reconfigure(encoding="utf-8")
    sys.stdout.write(md)
    return 0


if __name__ == "__main__":
    sys.exit(main())
