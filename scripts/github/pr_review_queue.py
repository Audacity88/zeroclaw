#!/usr/bin/env python3
"""Materialize report-only pull-request review queues from live GitHub state."""

from __future__ import annotations

import argparse
from concurrent.futures import FIRST_COMPLETED, ThreadPoolExecutor, wait
import json
import math
import re
import subprocess
import sys
import unicodedata
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Iterable
from urllib.parse import quote_plus

REPOSITORY = "zeroclaw-labs/zeroclaw"
CORE_ROSTER_PATH = Path(__file__).resolve().parents[2] / "docs/book/src/contributing/communication.md"
QUEUE_NAMES = ("maintainer", "second-core", "author-action", "stacked", "mine", "all")
EXCLUDED_REVIEW_LABELS = {"needs-author-action", "status:blocked", "do-not-merge", "stacked"}
MAINTAINER_LABEL = "needs-maintainer-review"
AUTHOR_ACTION_LABEL = "needs-author-action"
STACKED_LABEL = "stacked"
DISCOVERY_LANES = ("maintainer", "author-action", "stacked")
FULL_DISCOVERY_LANES = ("maintainer", "mine", "second-core")
COMMON_DISCOVERY_FIELDS = "number,title,author,isDraft,labels,state,mergeable,reviewDecision"
FULL_DISCOVERY_FIELDS = "number,title,author,isDraft,labels,url,mergeable,mergeStateStatus,reviewDecision,statusCheckRollup,headRefOid,state"
MAX_DETAIL_WORKERS = 8
GH_TIMEOUT_SECONDS = 30
ReviewFacts = tuple[str, int, list[str], str | None, bool]


class GitHubCommandError(RuntimeError):
    """A read-only gh command failed and should be reported verbatim."""


def _retryable_gh_error(detail: str) -> bool:
    return bool(re.search(r"\bHTTP\s+502\b|\b502\s+Bad\s+Gateway\b|\bTLS\s+handshake\s+timeout\b", detail, re.IGNORECASE))


def run_gh(*args: str) -> Any:
    """Run a read-only GitHub CLI command and decode its JSON response."""
    for attempt in range(2):
        try:
            result = subprocess.run(
                ["gh", *args],
                check=True,
                capture_output=True,
                text=True,
                timeout=GH_TIMEOUT_SECONDS,
            )
        except subprocess.CalledProcessError as exc:
            detail = (exc.stderr or exc.stdout or str(exc)).strip()
            if attempt == 0 and _retryable_gh_error(detail):
                continue
            raise GitHubCommandError(f"gh command failed: {detail}") from exc
        except subprocess.TimeoutExpired as exc:
            if attempt == 0:
                continue
            raise GitHubCommandError(f"gh command timed out after {GH_TIMEOUT_SECONDS}s") from exc
        return json.loads(result.stdout or "null")
    raise AssertionError("run_gh exhausted without a result or error")


def flatten_pages(payload: Any, source: str = "GitHub response") -> list[dict[str, Any]]:
    if not isinstance(payload, list):
        raise GitHubCommandError(f"unexpected {source} shape: expected a JSON list")
    if payload and all(isinstance(page, list) for page in payload):
        items = [item for page in payload for item in page]
    else:
        items = payload
    if not all(isinstance(item, dict) for item in items):
        raise GitHubCommandError(f"unexpected {source} shape: expected JSON objects")
    return items


def validate_pr(pr: Any, source: str = "pull-request response", lane: str | None = None) -> dict[str, Any]:
    if not isinstance(pr, dict):
        raise GitHubCommandError(f"unexpected {source} shape: expected a JSON object")
    required = {
        "number",
        "title",
        "author",
        "isDraft",
        "labels",
        "state",
        "mergeable",
        "reviewDecision",
    }
    if lane is None or lane in FULL_DISCOVERY_LANES:
        required.update({"statusCheckRollup", "headRefOid"})
    missing = sorted(required - pr.keys())
    if missing:
        raise GitHubCommandError(f"incomplete {source}: missing {', '.join(missing)}")
    if isinstance(pr["number"], bool) or not isinstance(pr["number"], int) or pr["number"] <= 0:
        raise GitHubCommandError(f"invalid {source}: number must be a positive integer")
    if not isinstance(pr["title"], str) or not isinstance(pr["author"], dict) or not isinstance(pr["labels"], list):
        raise GitHubCommandError(f"invalid {source}: title, author, and labels have unexpected types")
    author = pr["author"]
    if not isinstance(author.get("login"), str) or not author["login"]:
        raise GitHubCommandError(f"invalid {source}: author.login must be a non-empty string")
    if not isinstance(pr["state"], str) or not isinstance(pr["isDraft"], bool):
        raise GitHubCommandError(f"invalid {source}: missing state or draft metadata")
    label_names(pr)
    if pr["mergeable"] is not None and not isinstance(pr["mergeable"], str):
        raise GitHubCommandError(f"invalid {source}: mergeable has unexpected type")
    if pr["reviewDecision"] is not None and not isinstance(pr["reviewDecision"], str):
        raise GitHubCommandError(f"invalid {source}: reviewDecision has unexpected type")
    if "statusCheckRollup" in pr and pr["statusCheckRollup"] is not None and not isinstance(pr["statusCheckRollup"], (list, dict)):
        raise GitHubCommandError(f"invalid {source}: statusCheckRollup has unexpected type")
    if "headRefOid" in pr and pr["headRefOid"] is not None and not isinstance(pr["headRefOid"], str):
        raise GitHubCommandError(f"invalid {source}: headRefOid has unexpected type")
    return pr


def sanitize_text(value: Any) -> str:
    if value is None:
        return "?"
    text = str(value)
    escaped: list[str] = []
    bidi = {"LRE", "RLE", "LRO", "RLO", "PDF", "LRI", "RLI", "FSI", "PDI"}
    for character in text:
        if character == "\n":
            escaped.append("\\n")
        elif character == "\r":
            escaped.append("\\r")
        elif character == "\t":
            escaped.append("\\t")
        elif unicodedata.category(character).startswith("C") or unicodedata.bidirectional(character) in bidi:
            escaped.append(f"\\u{ord(character):04x}")
        else:
            escaped.append(character)
    return "".join(escaped)


def same_login(left: str | None, right: str | None) -> bool:
    return bool(left and right and left.casefold() == right.casefold())


def label_names(pr: dict[str, Any]) -> set[str]:
    labels = pr.get("labels")
    if not isinstance(labels, list):
        raise GitHubCommandError("invalid pull-request labels: expected a JSON list")
    names: set[str] = set()
    for index, label in enumerate(labels):
        if isinstance(label, str) and label:
            names.add(label)
        elif isinstance(label, dict) and isinstance(label.get("name"), str) and label["name"]:
            names.add(label["name"])
        else:
            raise GitHubCommandError(
                f"invalid pull-request labels: entry {index} must be a non-empty string or object with a non-empty string name"
            )
    return names


def author_login(pr: dict[str, Any]) -> str | None:
    author = pr.get("author")
    if isinstance(author, str):
        return author
    if isinstance(author, dict):
        login = author.get("login") or author.get("name")
        return login if isinstance(login, str) else None
    return None


def parse_timestamp(value: Any) -> datetime | None:
    if not isinstance(value, str) or not value:
        return None
    try:
        return datetime.fromisoformat(value.replace("Z", "+00:00")).astimezone(timezone.utc)
    except ValueError:
        return None


def timestamp(event: dict[str, Any]) -> datetime | None:
    for key in ("submitted_at", "submittedAt", "created_at", "createdAt", "authored_at", "authoredAt", "date"):
        parsed = parse_timestamp(event.get(key))
        if parsed:
            return parsed
    return None


def load_core_roster(path: Path = CORE_ROSTER_PATH) -> set[str]:
    """Read Core handles from the published maintainer summary."""
    roster: set[str] = set()
    for line in path.read_text().splitlines():
        if not line.startswith("|") or "|---" in line:
            continue
        first_cell = line.split("|", 2)[1]
        for token in first_cell.split("@")[1:]:
            handle = token.split("]", 1)[0].strip()
            if handle:
                roster.add(handle)
    return roster


def current_review_states(reviews: Iterable[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    """Reduce review history to the latest submitted state for each reviewer."""
    latest: dict[str, tuple[str, dict[str, Any]]] = {}
    for review in reviews:
        if not isinstance(review, dict):
            continue
        user = review.get("user") or review.get("author") or {}
        login = user if isinstance(user, str) else user.get("login") if isinstance(user, dict) else None
        if not isinstance(login, str) or not login:
            continue
        normalized_login = login.casefold()
        prior_item = latest.get(normalized_login)
        prior = prior_item[1] if prior_item else None
        current_key = (timestamp(review) or datetime.min.replace(tzinfo=timezone.utc), review.get("id", 0))
        prior_key = (timestamp(prior) or datetime.min.replace(tzinfo=timezone.utc), prior.get("id", 0)) if prior else None
        if prior is None or current_key >= prior_key:
            latest[normalized_login] = (login, review)
    return {display_login: review for display_login, review in latest.values()}


def _review_facts(
    pr: dict[str, Any], reviews: Iterable[dict[str, Any]], core_roster: set[str]
) -> ReviewFacts:
    states = current_review_states(reviews)
    head_sha = pr.get("headRefOid") or pr.get("head_sha") or pr.get("headSha")
    applicable: list[str] = []
    unknown: list[str] = []
    has_core_approval = False
    core_logins = {login.casefold() for login in core_roster}
    for login, review in states.items():
        if login.casefold() not in core_logins:
            continue
        state = str(review.get("state", "")).upper()
        if state != "APPROVED":
            continue
        has_core_approval = True
        review_commit = review.get("commit_id") or review.get("commitId")
        if not review_commit:
            unknown.append(f"Core approval by @{login} has no commit SHA")
            continue
        if not head_sha:
            unknown.append(f"Core approval by @{login} cannot be matched without the PR head SHA")
            continue
        if review_commit != head_sha:
            unknown.append(f"Core approval by @{login} targets an older head ({review_commit[:12]})")
            continue
        applicable.append(login)

    states_values = {str(review.get("state", "")).upper() for review in states.values()}
    decision = pr.get("reviewDecision") or pr.get("review_decision")
    if not isinstance(decision, str):
        if states_values:
            if "CHANGES_REQUESTED" in states_values:
                decision = "CHANGES_REQUESTED"
            elif applicable:
                decision = "APPROVED"
            else:
                decision = "REVIEW_REQUIRED"
        else:
            decision = "UNKNOWN"
    decision = str(decision).upper()
    count = len(applicable)
    note = "; ".join(unknown) if unknown else None
    return decision, count, sorted(applicable), note, has_core_approval


def review_facts(
    pr: dict[str, Any], reviews: Iterable[dict[str, Any]], core_roster: set[str]
) -> tuple[str, int, list[str], str | None]:
    decision, count, approvers, note, _ = _review_facts(pr, reviews, core_roster)
    return decision, count, approvers, note


def mergeability(pr: dict[str, Any]) -> str:
    value = pr.get("mergeable")
    return str(value).upper() if value else "UNKNOWN"


def required_gate_state(pr: dict[str, Any], checked: bool = True) -> str:
    if not checked:
        return "NOT_CHECKED"
    checks = pr.get("statusCheckRollup") or pr.get("status_check_rollup") or []
    if isinstance(checks, dict):
        checks = checks.get("contexts", [])
    candidates = []
    for check in checks if isinstance(checks, list) else []:
        if not isinstance(check, dict):
            continue
        name = check.get("name") or check.get("context") or check.get("workflowName") or ""
        if "required gate" in str(name).lower() or str(name).lower() == "required-gate":
            candidates.append(check)
    if not candidates:
        return "UNKNOWN"
    states: set[str] = set()
    for check in candidates:
        conclusion = str(check.get("conclusion") or "").upper()
        status = str(check.get("status") or "").upper()
        if conclusion in {"FAILURE", "CANCELLED", "TIMED_OUT", "ACTION_REQUIRED", "STALE"}:
            states.add("FAILURE")
        elif conclusion == "SUCCESS":
            states.add("SUCCESS")
        elif status in {"QUEUED", "IN_PROGRESS", "PENDING", "WAITING", "REQUESTED"}:
            states.add("PENDING")
        else:
            states.add("UNKNOWN")
    if "FAILURE" in states:
        return "FAILURE"
    if "PENDING" in states:
        return "PENDING"
    if "UNKNOWN" in states:
        return "UNKNOWN"
    return "SUCCESS"


def timeline_label_events(timeline: Iterable[dict[str, Any]], wanted: str) -> list[datetime]:
    starts: list[datetime] = []
    active_start: datetime | None = None
    for event in timeline:
        if not isinstance(event, dict):
            continue
        event_type = str(event.get("event") or event.get("type") or "").lower()
        label = event.get("label")
        label_name = label if isinstance(label, str) else label.get("name") if isinstance(label, dict) else None
        if event_type == "labeled" and label_name == wanted:
            event_time = timestamp(event)
            if event_time and active_start is None:
                active_start = event_time
        elif event_type == "unlabeled" and label_name == wanted and active_start is not None:
            starts.append(active_start)
            active_start = None
    if active_start is not None:
        starts.append(active_start)
    return starts


def actor_login(event: dict[str, Any]) -> str | None:
    actor = event.get("actor") or event.get("user") or event.get("author")
    if isinstance(actor, str):
        return actor
    if isinstance(actor, dict):
        login = actor.get("login") or actor.get("name")
        return login if isinstance(login, str) else None
    return None


def author_response_times(
    timeline: Iterable[dict[str, Any]], author: str | None
) -> list[datetime]:
    if not author:
        return []
    responses: list[datetime] = []
    for event in timeline:
        if not isinstance(event, dict) or not same_login(actor_login(event), author):
            continue
        event_type = str(event.get("event") or event.get("type") or "").lower()
        if event_type in {"commented", "committed", "reviewed"}:
            event_time = timestamp(event)
            if event_time:
                responses.append(event_time)
    return responses


def wait_start(
    queue: str,
    pr: dict[str, Any],
    timeline: Iterable[dict[str, Any]],
) -> tuple[datetime | None, str | None]:
    if queue == "author-action":
        starts = timeline_label_events(timeline, AUTHOR_ACTION_LABEL)
        if not starts:
            return None, "needs-author-action start is not present in timeline data"
        responses = author_response_times(timeline, author_login(pr))
        unanswered = [start for start in starts if not any(response > start for response in responses)]
        if not unanswered:
            return None, "author activity occurred after the request; unanswered clock is uncertain because activity does not prove every finding was addressed"
        return max(unanswered), None
    label = STACKED_LABEL if queue == "stacked" else MAINTAINER_LABEL
    starts = timeline_label_events(timeline, label)
    return (max(starts), None) if starts else (None, f"{label} start is not present in timeline data")


def age_days(start: datetime | None, now: datetime) -> float | None:
    if not start:
        return None
    return round(max(0.0, (now - start).total_seconds() / 86400), 1)


def base_maintainer_candidate(pr: dict[str, Any]) -> bool:
    labels = label_names(pr)
    return (
        str(pr.get("state", "open")).lower() == "open"
        and not bool(pr.get("isDraft") or pr.get("draft"))
        and MAINTAINER_LABEL in labels
        and not (labels & EXCLUDED_REVIEW_LABELS)
    )


def base_author_action_candidate(pr: dict[str, Any]) -> bool:
    labels = label_names(pr)
    return (
        str(pr.get("state", "open")).lower() == "open"
        and not bool(pr.get("isDraft") or pr.get("draft"))
        and AUTHOR_ACTION_LABEL in labels
        and not ({"status:blocked", "do-not-merge"} & labels)
    )


def build_row(
    queue: str,
    pr: dict[str, Any],
    reviews: list[dict[str, Any]],
    timeline: list[dict[str, Any]],
    core_roster: set[str],
    now: datetime,
    older_than_days: float,
    facts: ReviewFacts | None = None,
) -> dict[str, Any]:
    decision, core_count, core_approvers, review_note, _ = facts if facts is not None else _review_facts(pr, reviews, core_roster)
    start, wait_note = wait_start(queue, pr, timeline)
    days = age_days(start, now)
    labels = sorted(label_names(pr))
    gate_state = required_gate_state(pr, checked=queue in FULL_DISCOVERY_LANES)
    notes: list[str] = []
    eligible = True
    if queue in {"maintainer", "mine"}:
        if mergeability(pr) != "MERGEABLE":
            notes.append(f"mergeability is {mergeability(pr).lower()}")
            eligible = False
        if gate_state != "SUCCESS":
            notes.append(f"Required Gate is {gate_state.lower()}")
            eligible = False
        if wait_note:
            notes.append(f"needs-maintainer-review wait clock unknown: {wait_note}")
            eligible = False
    elif queue == "second-core":
        if core_count >= 2:
            notes.append("already has two current Core approvals")
            eligible = False
        if not core_approvers:
            notes.append("no proven-current Core approval; applicability is unknown")
            eligible = False
        if mergeability(pr) != "MERGEABLE":
            notes.append(f"mergeability is {mergeability(pr).lower()}")
            eligible = False
        if gate_state != "SUCCESS":
            notes.append(f"Required Gate is {gate_state.lower()}")
            eligible = False
        if wait_note:
            notes.append(f"needs-maintainer-review wait clock unknown: {wait_note}")
            eligible = False
    elif queue == "author-action":
        if days is None:
            notes.append(f"author-action wait clock unknown: {wait_note or 'no unanswered request evidence'}")
            eligible = False
        elif days < older_than_days:
            notes.append(f"wait clock is {days:g} days, below threshold {older_than_days:g}")
            eligible = False
    elif queue == "stacked" and wait_note:
        notes.append(wait_note)
    if review_note and review_note not in notes:
        notes.append(review_note)
    if queue == "second-core" and core_approvers:
        notes.append(f"current Core approvals: {', '.join('@' + login for login in core_approvers)}")
    return {
        "number": pr.get("number"),
        "title": sanitize_text(pr.get("title", "")),
        "author": sanitize_text(author_login(pr)),
        "queue": queue,
        "wait_start": start.isoformat().replace("+00:00", "Z") if start else None,
        "wait_days": days,
        "mergeability": mergeability(pr),
        "required_gate": gate_state,
        "review_decision": decision,
        "core_approvals": core_count,
        "labels": labels,
        "url": f"https://github.com/{REPOSITORY}/pull/{pr['number']}",
        "eligibility": "eligible" if eligible else "unknown" if any("unknown" in note.lower() for note in notes) else "ineligible",
        "note": sanitize_text("; ".join(notes) or "matches queue criteria"),
    }


def fetch_pr_details(pr: dict[str, Any], gh: Callable[..., Any] = run_gh) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
    number = str(pr["number"])
    reviews = flatten_pages(gh("api", f"repos/{REPOSITORY}/pulls/{number}/reviews", "--paginate", "--slurp"), "review response")
    timeline = flatten_pages(gh("api", f"repos/{REPOSITORY}/issues/{number}/timeline", "--paginate", "--slurp"), "timeline response")
    return reviews, timeline


def discovery_search(queue: str, author: str | None = None) -> str:
    """Return the narrow GitHub search fragment used by ``gh pr list``."""
    base = "draft:false"
    if queue in {"maintainer", "mine", "second-core"}:
        query = f"{base} label:{MAINTAINER_LABEL} -label:needs-author-action -label:status:blocked -label:do-not-merge -label:stacked"
        if queue == "mine":
            if not author:
                raise ValueError("--author is required for the mine discovery lane")
            query += f" author:{author}"
        if queue == "second-core":
            query += ' label:"risk:high","domain:security"'
        return query
    if queue == "author-action":
        return f"{base} label:{AUTHOR_ACTION_LABEL} -label:status:blocked -label:do-not-merge"
    if queue == "stacked":
        return f"{base} label:{STACKED_LABEL}"
    raise ValueError(f"queue {queue!r} is not a single discovery lane")


def _fetch_pull_request_lane(
    queue: str, author: str | None, gh: Callable[..., Any]
) -> list[dict[str, Any]]:
    payload = gh(
        "pr",
        "list",
        "--repo",
        REPOSITORY,
        "--state",
        "open",
        "--search",
        discovery_search(queue, author),
        "--limit",
        "1000",
        "--json",
        FULL_DISCOVERY_FIELDS if queue in FULL_DISCOVERY_LANES else COMMON_DISCOVERY_FIELDS,
    )
    if not isinstance(payload, list):
        raise GitHubCommandError("unexpected pull-request list response shape: expected a JSON list")
    return [validate_pr(pr, "pull-request response", queue) for pr in payload]


def discovery_common_facts(pr: dict[str, Any]) -> tuple[Any, ...]:
    """Return the lane-independent facts used to validate an all-lane overlap."""
    return (
        pr["number"],
        pr["title"],
        author_login(pr).casefold(),
        pr["isDraft"],
        frozenset(label_names(pr)),
        pr["state"].upper(),
        pr["mergeable"].upper() if isinstance(pr["mergeable"], str) else None,
        pr["reviewDecision"].upper() if isinstance(pr["reviewDecision"], str) else None,
    )


def fetch_pull_requests(
    queue: str, author: str | None = None, gh: Callable[..., Any] = run_gh
) -> list[dict[str, Any]]:
    lanes = DISCOVERY_LANES if queue == "all" else (queue,)
    by_number: dict[Any, dict[str, Any]] = {}
    first_lane_by_number: dict[Any, str] = {}
    for lane in lanes:
        for pr in _fetch_pull_request_lane(lane, author, gh):
            number = pr.get("number")
            first_lane = first_lane_by_number.get(number)
            if queue == "all" and first_lane is not None and first_lane != lane:
                if discovery_common_facts(by_number[number]) != discovery_common_facts(pr):
                    raise GitHubCommandError(
                        f"all-lane discovery snapshot changed: PR #{number} returned conflicting common facts in {first_lane} and {lane}; rerun the queue command"
                    )
                continue
            if number not in by_number:
                by_number[number] = pr
                first_lane_by_number[number] = lane
    return list(by_number.values())


def queue_candidates(queue: str, pr: dict[str, Any], author: str | None) -> bool:
    labels = label_names(pr)
    if queue == "maintainer":
        return base_maintainer_candidate(pr)
    if queue == "mine":
        return base_maintainer_candidate(pr) and same_login(author_login(pr), author)
    if queue == "author-action":
        return base_author_action_candidate(pr)
    if queue == "stacked":
        return str(pr.get("state", "open")).lower() == "open" and not bool(pr.get("isDraft") or pr.get("draft")) and STACKED_LABEL in labels
    if queue == "second-core":
        return base_maintainer_candidate(pr) and bool({"risk:high", "domain:security"} & labels)
    raise ValueError(f"unknown queue {queue}")


def collect_rows(
    queue: str,
    prs: list[dict[str, Any]],
    older_than_days: float,
    author: str | None,
    now: datetime | None = None,
    gh: Callable[..., Any] = run_gh,
    core_roster: set[str] | None = None,
) -> list[dict[str, Any]]:
    now = now or datetime.now(timezone.utc)
    core_roster = core_roster if core_roster is not None else load_core_roster()
    queues = ("maintainer", "second-core", "author-action", "stacked", "mine") if queue == "all" else (queue,)
    candidate_prs = {
        pr["number"]: pr
        for pr in prs
        if any(queue_candidates(lane, pr, author) for lane in queues)
    }
    details_by_number: dict[int, tuple[list[dict[str, Any]], list[dict[str, Any]]]] = {}
    if candidate_prs:
        executor = ThreadPoolExecutor(max_workers=min(MAX_DETAIL_WORKERS, len(candidate_prs)))
        pending: dict[Any, int] = {}
        candidate_iter = iter(candidate_prs.items())

        def submit_next() -> bool:
            try:
                number, pr = next(candidate_iter)
            except StopIteration:
                return False
            pending[executor.submit(fetch_pr_details, pr, gh)] = number
            return True

        try:
            for _ in range(min(MAX_DETAIL_WORKERS, len(candidate_prs))):
                submit_next()
            while pending:
                done, _ = wait(tuple(pending), return_when=FIRST_COMPLETED)
                failure: Exception | None = None
                for future in done:
                    number = pending.pop(future)
                    try:
                        details_by_number[number] = future.result()
                    except Exception as exc:
                        failure = failure or exc
                if failure is not None:
                    for future in pending:
                        future.cancel()
                    raise failure
                for _ in done:
                    submit_next()
        finally:
            executor.shutdown(wait=True)
    rows: list[dict[str, Any]] = []
    for pr in candidate_prs.values():
        details = details_by_number[pr["number"]]
        for lane in queues:
            if not queue_candidates(lane, pr, author):
                continue
            reviews, timeline = details
            facts = _review_facts(pr, reviews, core_roster)
            row = build_row(lane, pr, reviews, timeline, core_roster, now, older_than_days, facts=facts)
            if lane == "second-core":
                decision, count, _, _, has_core_approval = facts
                if decision != "APPROVED":
                    continue
                if not has_core_approval:
                    continue
                if count >= 2:
                    continue
            if lane == "author-action" and row["wait_days"] is not None and row["wait_days"] < older_than_days:
                continue
            rows.append(row)
    return sorted(rows, key=lambda row: (row["queue"], row["number"] or 0))


def search_query(queue: str, author: str | None, older_than_days: float) -> str:
    base = "repo:{repo} is:pr is:open draft:false".format(repo=REPOSITORY)
    if queue in {"maintainer", "mine", "second-core"}:
        query = f"{base} label:{MAINTAINER_LABEL} -label:needs-author-action -label:status:blocked -label:do-not-merge -label:stacked"
        if queue == "mine":
            query += f" author:{author or '<author>'}"
        if queue == "second-core":
            query += ' label:"risk:high","domain:security"'
    elif queue == "author-action":
        query = f"{base} label:{AUTHOR_ACTION_LABEL} -label:status:blocked -label:do-not-merge"
    elif queue == "stacked":
        query = f"{base} label:{STACKED_LABEL}"
    else:
        query = f"repo:{REPOSITORY} is:pr is:open draft:false"
    return query


def render_links(queue: str, rows: list[dict[str, Any]], author: str | None, older_than_days: float) -> str:
    lanes = (
        tuple(lane for lane in QUEUE_NAMES[:-1] if lane != "mine" or author)
        if queue == "all"
        else (queue,)
    )
    lines: list[str] = []
    for lane in lanes:
        lines.append(f"{lane}: https://github.com/{REPOSITORY}/pulls?q={quote_plus(search_query(lane, author, older_than_days))}")
        lines.extend(f"#{row['number']} {row['url']}" for row in rows if row["queue"] == lane)
    if queue == "all" and not author:
        lines.append("mine: omitted (pass --author LOGIN to include it)")
    return "\n".join(lines) + "\n"


def render_table(rows: list[dict[str, Any]]) -> str:
    headers = ["NUMBER", "QUEUE", "AUTHOR", "WAIT", "MERGE", "GATE", "REVIEW", "ELIGIBILITY", "TITLE", "URL", "NOTE"]
    values = [
        [
            str(row["number"]),
            row["queue"],
            row["author"] or "?",
            f"{row['wait_days']:g}d" if row["wait_days"] is not None else "?",
            row["mergeability"],
            row["required_gate"],
            row["review_decision"],
            row["eligibility"],
            row["title"].replace("\n", " "),
            row["url"],
            row["note"],
        ]
        for row in rows
    ]
    widths = [max(len(headers[index]), *(len(value[index]) for value in values)) for index in range(len(headers))] if values else [len(header) for header in headers]
    lines = ["  ".join(header.ljust(widths[index]) for index, header in enumerate(headers))]
    lines.append("  ".join("-" * width for width in widths))
    lines.extend("  ".join(value[index].ljust(widths[index]) for index in range(len(headers))) for value in values)
    return "\n".join(lines) + "\n"


def nonnegative_finite_float(value: str) -> float:
    try:
        parsed = float(value)
    except ValueError as exc:
        raise argparse.ArgumentTypeError("must be a finite non-negative number") from exc
    if not math.isfinite(parsed) or parsed < 0:
        raise argparse.ArgumentTypeError("must be a finite non-negative number")
    return parsed


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--queue", choices=QUEUE_NAMES, required=True)
    parser.add_argument("--older-than-days", type=nonnegative_finite_float, default=7)
    parser.add_argument("--author", help="GitHub login for the mine queue.")
    parser.add_argument("--format", choices=("table", "json", "links"), default="table")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None, gh: Callable[..., Any] = run_gh) -> int:
    args = parse_args(argv)
    if args.queue == "mine" and not args.author:
        print("--author is required for --queue mine", file=sys.stderr)
        return 2
    if args.queue == "all" and not args.author:
        print("Note: mine queue omitted; pass --author LOGIN to include it.", file=sys.stderr)
    try:
        rows = collect_rows(args.queue, fetch_pull_requests(args.queue, args.author, gh), args.older_than_days, args.author, gh=gh)
    except (OSError, GitHubCommandError, subprocess.CalledProcessError, json.JSONDecodeError, ValueError) as exc:
        print(f"Failed to read GitHub state: {exc}", file=sys.stderr)
        return 1
    if args.format == "json":
        print(json.dumps(rows, indent=2, sort_keys=True))
    elif args.format == "links":
        print(render_links(args.queue, rows, args.author, args.older_than_days), end="")
    else:
        print(render_table(rows), end="")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
