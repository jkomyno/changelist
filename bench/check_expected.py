"""Second, independent computation of the benchmark's expected outputs.

Reads the dataset dump written by bench/dump-datasets.ts and recomputes every
success scenario's expected output with Python's standard library only. It parses
timestamps as datetimes (the TypeScript oracle and the guest programs compare ISO
strings lexically) and derives the staleness cutoff from `now` itself.

    python3 -I bench/check_expected.py <datasets.json> <expected.json>

Exits 1 and prints the differing scenarios if the files disagree.
"""

import json
import sys
from collections import Counter
from datetime import datetime, timedelta, timezone

PRIORITY = {"p0": 0, "p1": 1, "p2": 2, "p3": 3}


def ts(text):
    parsed = datetime.strptime(text, "%Y-%m-%dT%H:%M:%SZ")
    return parsed.replace(tzinfo=timezone.utc)


def triage(data, deny_confidential):
    a = data["A"]
    cutoff = ts(data["now"]) - timedelta(days=7)
    users = {user["id"]: user for user in a["users"]}
    urgent = [
        issue
        for issue in a["details"]
        if issue["state"] == "open"
        and "regression" in issue["labels"]
        and issue["priority"] in ("p0", "p1")
    ]
    denied = sorted(issue["id"] for issue in urgent if issue.get("confidential")) if deny_confidential else []
    stale = []
    for issue in urgent:
        if issue["id"] in denied:
            continue
        times = [ts(comment["at"]) for comment in issue["comments"]]
        last = max(times) if times else ts(issue["updated_at"])
        if last < cutoff and not users[issue["assignee"]]["on_call"]:
            stale.append(issue)
    stale.sort(key=lambda issue: (PRIORITY[issue["priority"]], ts(issue["updated_at"]), issue["id"]))
    output = {
        "stale_count": len(stale),
        "top": [
            {
                "id": issue["id"],
                "title": issue["title"],
                "owner": users[issue["assignee"]]["name"],
                "team": users[issue["assignee"]]["team"],
            }
            for issue in stale[:5]
        ],
        "by_team": dict(Counter(users[issue["assignee"]]["team"] for issue in stale)),
    }
    if deny_confidential:
        output["denied"] = denied
    return output


def release_audit(data):
    b = data["B"]
    start = ts(data["release_window"]["start"])
    end = ts(data["release_window"]["end"])
    teams = {user["id"]: user["team"] for user in data["A"]["users"]}
    merged = [pull for pull in b["pulls"] if pull["merged_at"] and start <= ts(pull["merged_at"]) < end]
    by_team, by_check, failing = Counter(), Counter(), []
    for pull in merged:
        failures = [check for check in b["checks"][str(pull["number"])] if check["conclusion"] == "failure"]
        if not failures:
            continue
        failing.append(pull["number"])
        by_team[teams[pull["author"]]] += len(failures)
        by_check.update(check["name"] for check in failures)
    return {
        "window": dict(data["release_window"]),
        "merged_count": len(merged),
        "failing_prs": sorted(failing),
        "failing_by_team": dict(by_team),
        "failing_by_check": dict(by_check),
    }


def canonical(value):
    return json.dumps(value, sort_keys=True, separators=(",", ":"))


def main():
    if len(sys.argv) != 3:
        sys.exit("usage: check_expected.py <datasets.json> <expected.json>")
    with open(sys.argv[1], encoding="utf-8") as handle:
        data = json.load(handle)
    with open(sys.argv[2], encoding="utf-8") as handle:
        expected = json.load(handle)
    computed = {
        "triage": triage(data, False),
        "triage-large": triage(data, False),
        "denied": triage(data, True),
        "release-audit": release_audit(data),
    }
    bad = sorted(set(computed) ^ set(expected))
    bad += [name for name in computed if name in expected and canonical(computed[name]) != canonical(expected[name])]
    if bad:
        for name in bad:
            print(f"check_expected: {name} differs", file=sys.stderr)
            print(f"  python: {canonical(computed.get(name))}", file=sys.stderr)
            print(f"  file:   {canonical(expected.get(name))}", file=sys.stderr)
        sys.exit(1)
    print(f"check_expected: {len(computed)} scenarios agree")


if __name__ == "__main__":
    main()
