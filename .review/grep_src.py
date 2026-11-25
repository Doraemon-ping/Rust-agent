import os, re, sys, io
sys.stdout = io.TextIOWrapper(sys.stdout.buffer, encoding="utf-8", errors="replace")
pats = ["max_steps", "todo_reminder_after", "consolidate_threshold", "IDLE_SCAN_INTERVAL",
        "WORKFLOW_AGENT_MAX_TOKENS", "AGENT_CAP", "CONCURRENCY", "recall_char_limit",
        "max_selected", "SKILL_MANIFEST", "WORKFLOW_TOOL_NAME", "REVIEW_DIMENSIONS",
        "max_reactive_retries", "fallback", "0.25", "25%", "cleanup", "worktree"]
root = r"D:\Agent\Rust-agent"
hits = {p: [] for p in pats}
for dp, dn, fn in os.walk(os.path.join(root, "src")):
    for f in fn:
        if not f.endswith(".rs"):
            continue
        fp = os.path.join(dp, f)
        try:
            lines = open(fp, encoding="utf-8", errors="replace").read().splitlines()
        except Exception:
            continue
        for i, ln in enumerate(lines, 1):
            for p in pats:
                if p.lower() in ln.lower():
                    hits[p].append((os.path.relpath(fp, root), i, ln.strip()[:160]))
for p in pats:
    print("#### " + p)
    for h in hits[p][:12]:
        print("   %s:%d  %s" % h)
    if len(hits[p]) > 12:
        print("   ... (%d more)" % (len(hits[p]) - 12))
