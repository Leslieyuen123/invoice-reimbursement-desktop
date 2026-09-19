#!/usr/bin/env python3
"""Push the current commit without `git push`.

`git push` over HTTPS to github.com is unreliable on this machine (recv
timeouts), so this script builds the same commit through the GitHub Git Data
API with the authenticated `gh` CLI and then moves the branch ref.

It refuses to run unless the content matches: the remote tip must either be the
local parent, or a commit with the local parent's tree (a commit created through
this same API has no local committer metadata, so the same content has a
different SHA). The generated tree is compared with `git rev-parse HEAD^{tree}`
before the ref is updated.

Usage: python3 scripts/push-via-github-api.py
"""
import base64, json, subprocess, sys, tempfile, os

REPO = os.environ.get("INVOICE_REPO", "Leslieyuen123/invoice-reimbursement-desktop")

def git(*args):
    return subprocess.run(["git", *args], capture_output=True, text=True, check=True).stdout.strip()

def gh(path, payload=None, method="GET"):
    cmd = ["gh", "api", path]
    if method != "GET":
        cmd += ["--method", method]
    tmp = None
    if payload is not None:
        fd, tmp = tempfile.mkstemp(suffix=".json")
        with os.fdopen(fd, "w") as fh:
            json.dump(payload, fh)
        cmd += ["--input", tmp]
    try:
        out = subprocess.run(cmd, capture_output=True, text=True)
        if out.returncode != 0:
            print("gh api failed:", path, out.stderr[:800])
            sys.exit(1)
        return json.loads(out.stdout) if out.stdout.strip() else {}
    finally:
        if tmp:
            os.unlink(tmp)

branch = git("branch", "--show-current")
head = git("rev-parse", "HEAD")
parent = git("rev-parse", "HEAD^")
local_tree = git("rev-parse", "HEAD^{tree}")
print("branch:", branch, "head:", head[:8], "parent:", parent[:8])

try:
    ref = gh(f"repos/{REPO}/git/ref/heads/{branch}")
    remote_sha = ref["object"]["sha"]
except SystemExit:
    remote_sha = None
print("remote tip:", remote_sha[:8] if remote_sha else "absent")

if remote_sha == head:
    print("already pushed")
    sys.exit(0)
parent_tree = git("rev-parse", "HEAD^^{tree}")
base_tree = gh(f"repos/{REPO}/git/commits/{remote_sha}")["tree"]["sha"]
if remote_sha != parent:
    # A commit created through this API has no local committer metadata, so the
    # same content has a different SHA. Only continue when the content matches.
    if base_tree != parent_tree:
        print("STOP: remote tip has different content; refusing to build a commit blindly")
        sys.exit(2)
    print("remote tip is the API twin of the local parent (same tree)")

changed = [line.split("\t") for line in git("diff", "--name-status", parent, head).splitlines()]
print("changed files:", len(changed))

entries = []
for status, path in [(c[0], c[1]) for c in changed]:
    if status == "D":
        entries.append({"path": path, "mode": "100644", "type": "blob", "sha": None})
        continue
    with open(path, "rb") as fh:
        content = base64.b64encode(fh.read()).decode()
    mode = "100755" if os.access(path, os.X_OK) else "100644"
    blob = gh(f"repos/{REPO}/git/blobs", {"content": content, "encoding": "base64"}, "POST")
    entries.append({"path": path, "mode": mode, "type": "blob", "sha": blob["sha"]})
    print("  blob", path, blob["sha"][:8])

tree = gh(f"repos/{REPO}/git/trees", {"base_tree": base_tree, "tree": entries}, "POST")
print("tree:", tree["sha"][:8], "local:", local_tree[:8])
if tree["sha"] != local_tree:
    print("STOP: generated tree differs from the local commit tree")
    sys.exit(3)

message = git("log", "-1", "--pretty=%B")
commit = gh(f"repos/{REPO}/git/commits", {"message": message, "tree": tree["sha"], "parents": [remote_sha]}, "POST")
print("commit:", commit["sha"])
gh(f"repos/{REPO}/git/refs/heads/{branch}", {"sha": commit["sha"], "force": False}, "PATCH")
print("PUSHED", commit["sha"][:8], "to", branch)
