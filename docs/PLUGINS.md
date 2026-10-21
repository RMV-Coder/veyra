# Analyzer plugins

`veyra analyze` flags risky database migrations and infrastructure changes with built-in rules.
A repository can add its own checks in the committed `.veyra/analyzers.toml`, in two ways:

- **Pattern rules** (`[[rule]]`): a regular expression matched against added lines. They are
  data, not code, so they always run.
- **Plugins** (`[[plugin]]`): any program that reads the change as JSON and answers with
  findings. Use them for checks a pattern can't express: parse a schema, call your linter, look
  something up.

## Declaring a plugin

```toml
[[plugin]]
id = "orm-lint"                              # unique, no `/`
command = ["python", "tools/orm_lint.py"]    # program and arguments
paths = ["app/models/**/*.py"]               # optional: the changed files it sees (default: all)
timeout_seconds = 30                         # optional (default 60)
```

A relative program path such as `tools/check` is relative to the repository root, and the
command runs from the repository root. On Windows, run scripts through their interpreter
(`["sh", "tools/check.sh"]`, `["python", "tools/check.py"]`).

## Trust

A plugin is a program, and `.veyra/analyzers.toml` is committed: anyone who can push to the
repository can change it. So Veyra runs a plugin only after **you** have trusted it in your
clone:

```console
$ veyra analyze
warning: plugin `orm-lint` isn't trusted here (it would run `python tools/orm_lint.py`)
  hint: review it in .veyra/analyzers.toml, then `veyra analyze --trust-plugin orm-lint`

$ veyra analyze --trust-plugin orm-lint
Trusted plugin `orm-lint`: it runs `python tools/orm_lint.py` from the repository root.
```

- Trust is recorded in `.git/veyra/trusted-plugins.json`, which is never committed or cloned.
- Trust covers the plugin's **exact definition**. If a commit you pull changes its command, its
  paths or its timeout, it stops running until you review and trust it again.
- Plugins never run in a repository you don't own (the same rule Veyra applies to Git hooks).
- `--revoke-plugin <id>` withdraws trust; `--no-plugins` skips all plugins for one run.
- In CI, the workflow file is under your control, so trusting there is explicit too:
  `veyra analyze --trust-plugin orm-lint --fail-on high`.

Trust covers the definition in `.veyra/analyzers.toml`, not the script it runs. If the script is
in the repository, review changes to it as you would any code that runs on your machine.

## Protocol

Veyra writes one JSON request to the plugin's standard input:

```json
{
  "version": 1,
  "plugin": "orm-lint",
  "files": [
    { "path": "app/models/user.py", "old": "…previous content…", "new": "…new content…" }
  ]
}
```

`old` is `null` for an added file and `new` is `null` for a deleted one. Binary files are sent as
`null` on both sides. If no changed file matches `paths`, the plugin isn't started at all.

The plugin answers on standard output, and exits with status 0:

```json
{
  "findings": [
    {
      "severity": "high",
      "rule": "nullable-fk",
      "path": "app/models/user.py",
      "line": 12,
      "message": "A new foreign key is nullable",
      "advice": "Backfill first, then add NOT NULL in a second migration",
      "excerpt": "team_id = Column(ForeignKey('teams.id'))"
    }
  ]
}
```

- `severity` is `high`, `medium` or `low`. `line` (1-based, 0 for the whole file), `advice` and
  `excerpt` are optional.
- `path` must be one of the files the plugin was given.
- Findings are reported as `<plugin id>/<rule>` (here `orm-lint/nullable-fk`), so
  `disable = ["orm-lint/nullable-fk"]` silences one rule and `disable = ["orm-lint"]` the whole
  plugin.

## Failures

A plugin that exits with a non-zero status, prints something other than the expected JSON, or
doesn't answer within its timeout produces a warning. With `--fail-on`, it also makes
`veyra analyze` fail, so a broken plugin can't let a change through CI unchecked.

## A minimal plugin

```python
#!/usr/bin/env python3
import json, sys

request = json.load(sys.stdin)
findings = []
for f in request["files"]:
    for i, line in enumerate((f["new"] or "").splitlines(), start=1):
        if "DROP TABLE" in line.upper():
            findings.append({"severity": "high", "rule": "drop-table", "path": f["path"],
                             "line": i, "message": "Dropping a table loses data"})
json.dump({"findings": findings}, sys.stdout)
```
