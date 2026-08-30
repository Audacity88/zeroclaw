#!/usr/bin/env python3
"""Evaluate the report-only pull-request risk and approval policy."""

from __future__ import annotations

import argparse
import base64
from datetime import datetime, timezone
from functools import lru_cache
import html
import json
import os
from pathlib import Path
import re
import sys
from typing import Any, Iterable
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlencode, urljoin
from urllib.request import Request, urlopen


DEFAULT_REPOSITORY = "zeroclaw-labs/zeroclaw"
DEFAULT_STATUS_CONTEXT = "zeroclaw/pr-risk-policy"
PAGE_SIZE = 100
MAX_PAGES = 1000
MAX_PR_FILES = 3000
HIGH_LABEL = "risk:high"
MANUAL_LABEL = "risk:manual"
SECURITY_LABEL = "domain:security"
RISK_LABELS = {"risk:low", "risk:medium", HIGH_LABEL, MANUAL_LABEL}
DECISIVE_STATES = {"APPROVED", "CHANGES_REQUESTED", "DISMISSED"}
NONDECISIVE_STATES = {"COMMENTED", "PENDING"}
LOW_PATH_GLOBS = (
    "docs/**",
    "**/*.md",
    "**/*.mdx",
    "LICENSE",
    ".markdownlint-cli2.yaml",
    "locales/**",
    "**/locales/**",
    "**/fixtures/**",
    "fixtures/**",
    "**/__fixtures__/**",
    ".editorconfig",
    ".gitattributes",
    ".gitignore",
)
RUST_SUFFIX = ".rs"
CFG_TEST_RE = re.compile(r"^\s*#\s*\[\s*cfg\s*\(\s*test\s*\)\s*\]\s*$")
CFG_BOUNDARY_RE = re.compile(r"(?:#\s*\[\s*cfg\b|\bcfg\s*(?:!|_attr\s*\())")
HUNK_RE = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")
HANDLE_RE = re.compile(r"^\[@([A-Za-z0-9][A-Za-z0-9-]*)\]\([^)]*\)$")
RAW_STRING_START_RE = re.compile(r'(?:br|cr|r)(?P<hashes>#{0,255})"')
CHAR_LITERAL_RE = re.compile(r"'(?:\\(?:[nrt0\\'\"]|x[0-9A-Fa-f]{2}|u\{[0-9A-Fa-f_]{1,6}\})|[^\\'\n])'")


class PolicyError(RuntimeError):
    """An API, policy, roster, patch, or source input was not trustworthy."""


def require(condition: bool, message: str) -> None:
    if not condition:
        raise PolicyError(message)


def normalize_path(value: str) -> str:
    require(isinstance(value, str), "invalid file path")
    path = value.replace("\\", "/")
    require(path and not path.startswith("/") and "\x00" not in path, "invalid file path")
    require(all(part not in {"", ".", ".."} for part in path.split("/")), "invalid file path")
    return path


def parse_json_file(path: Path, description: str) -> Any:
    try:
        with path.open(encoding="utf-8") as stream:
            return json.load(stream)
    except (OSError, UnicodeDecodeError, json.JSONDecodeError) as exc:
        raise PolicyError(f"invalid {description}") from exc


def load_policy(path: Path) -> tuple[str, ...]:
    payload = parse_json_file(path, "risk policy")
    require(isinstance(payload, dict) and set(payload) == {HIGH_LABEL}, "risk policy shape is invalid")
    rules = payload[HIGH_LABEL]
    require(isinstance(rules, list) and rules, "risk policy has no high-risk rules")
    globs: list[str] = []
    for rule in rules:
        require(isinstance(rule, dict) and set(rule) == {"changed-files"}, "risk policy rule is invalid")
        changed_files = rule["changed-files"]
        require(
            isinstance(changed_files, dict) and set(changed_files) == {"any-glob-to-any-file"},
            "risk policy changed-files rule is invalid",
        )
        values = changed_files["any-glob-to-any-file"]
        require(isinstance(values, list) and values, "risk policy glob list is empty")
        for value in values:
            require(isinstance(value, str) and value and "\x00" not in value, "risk policy glob is invalid")
            globs.append(value)
    require(len(globs) == len(set(globs)), "risk policy contains duplicate globs")
    return tuple(globs)


def parse_table_cells(line: str) -> list[str] | None:
    if not line.strip().startswith("|"):
        return None
    cells = [cell.strip() for cell in line.strip().strip("|").split("|")]
    return cells if cells else None


def load_core_roster(path: Path) -> set[str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeDecodeError) as exc:
        raise PolicyError("Core roster cannot be read") from exc

    table_starts: list[int] = []
    for index, line in enumerate(lines[:-1]):
        cells = parse_table_cells(line)
        next_cells = parse_table_cells(lines[index + 1])
        if not cells or len(cells) < 2 or not next_cells or len(next_cells) < 2:
            continue
        if cells[0].casefold() == "handle" and cells[1].casefold() == "role":
            if all(re.fullmatch(r":?-{3,}:?", cell) for cell in next_cells[: len(cells)]):
                table_starts.append(index + 2)
    require(len(table_starts) == 1, "Core roster table is missing or ambiguous")

    roster: set[str] = set()
    saw_core = False
    for line in lines[table_starts[0] :]:
        cells = parse_table_cells(line)
        if cells is None:
            break
        require(len(cells) >= 2, "Core roster row is malformed")
        role = cells[1]
        if not re.fullmatch(r"Core Team(?:,.*)?", role):
            continue
        saw_core = True
        match = HANDLE_RE.fullmatch(cells[0])
        require(match is not None, "Core roster handle is malformed")
        handle = match.group(1).casefold()
        require(handle not in roster, "Core roster contains a duplicate")
        roster.add(handle)
    require(saw_core and roster, "Core roster is empty")
    return roster


def labels_from_pr(pr: dict[str, Any]) -> set[str]:
    values = pr.get("labels")
    require(isinstance(values, list), "PR labels are malformed")
    names: set[str] = set()
    for value in values:
        require(isinstance(value, dict) and isinstance(value.get("name"), str) and value["name"], "PR label is malformed")
        names.add(value["name"])
    return names


def parse_sha(value: Any, description: str) -> str:
    require(isinstance(value, str) and re.fullmatch(r"[0-9a-fA-F]{40}", value) is not None, f"{description} is missing")
    return value


def parse_pr_metadata(pr: Any) -> tuple[str, str, set[str], int]:
    require(isinstance(pr, dict), "PR metadata is malformed")
    head = pr.get("head")
    base = pr.get("base")
    require(isinstance(head, dict) and isinstance(base, dict), "PR refs are malformed")
    changed_files = pr.get("changed_files")
    require(isinstance(changed_files, int) and changed_files >= 0, "PR changed-files count is malformed")
    return parse_sha(head.get("sha"), "PR head SHA"), parse_sha(base.get("sha"), "PR base SHA"), labels_from_pr(pr), changed_files


def parse_timestamp(value: Any) -> datetime:
    require(isinstance(value, str), "review timestamp is missing")
    try:
        parsed = datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError as exc:
        raise PolicyError("review timestamp is malformed") from exc
    require(parsed.tzinfo is not None, "review timestamp has no timezone")
    return parsed.astimezone(timezone.utc)


def review_login(review: dict[str, Any]) -> str:
    user = review.get("user")
    require(isinstance(user, dict) and isinstance(user.get("login"), str) and user["login"], "review author is malformed")
    return user["login"]


def validate_reviews(reviews: Iterable[Any]) -> list[dict[str, Any]]:
    result: list[dict[str, Any]] = []
    for review in reviews:
        require(isinstance(review, dict), "review record is malformed")
        require(isinstance(review.get("id"), int) and review["id"] >= 0, "review ID is malformed")
        state = review.get("state")
        require(isinstance(state, str) and state.upper() in DECISIVE_STATES | NONDECISIVE_STATES, "review state is malformed")
        review_login(review)
        if state.upper() in DECISIVE_STATES:
            parse_timestamp(review.get("submitted_at"))
            parse_sha(review.get("commit_id"), "review commit SHA")
        result.append(review)
    return result


def review_snapshot(reviews: Iterable[dict[str, Any]]) -> tuple[tuple[Any, ...], ...]:
    return tuple(
        sorted(
            (
                int(review["id"]),
                review_login(review).casefold(),
                str(review["state"]).upper(),
                review.get("commit_id"),
                review.get("submitted_at"),
            )
            for review in reviews
        )
    )


def latest_decisive_reviews(reviews: Iterable[dict[str, Any]]) -> dict[str, dict[str, Any]]:
    latest: dict[str, tuple[tuple[datetime, int], dict[str, Any]]] = {}
    for review in reviews:
        state = str(review["state"]).upper()
        if state not in DECISIVE_STATES:
            continue
        reviewer = review_login(review).casefold()
        key = (parse_timestamp(review["submitted_at"]), int(review.get("id") or 0))
        if reviewer not in latest or key >= latest[reviewer][0]:
            latest[reviewer] = (key, review)
    return {reviewer: review for reviewer, (_, review) in latest.items()}


def is_high_path(path: str, high_globs: Iterable[str]) -> list[str]:
    return [pattern for pattern in high_globs if glob_matches(path, pattern)]


@lru_cache(maxsize=None)
def glob_regex(pattern: str) -> re.Pattern[str]:
    parts: list[str] = []
    index = 0
    while index < len(pattern):
        if pattern.startswith("**/", index):
            parts.append("(?:.*/)?")
            index += 3
        elif pattern.startswith("**", index):
            parts.append(".*")
            index += 2
        elif pattern[index] == "*":
            parts.append("[^/]*")
            index += 1
        elif pattern[index] == "?":
            parts.append("[^/]")
            index += 1
        elif pattern[index] == "[":
            end = pattern.find("]", index + 1)
            if end == -1:
                parts.append(r"\[")
                index += 1
            else:
                parts.append(pattern[index : end + 1])
                index = end + 1
        else:
            parts.append(re.escape(pattern[index]))
            index += 1
    return re.compile("^" + "".join(parts) + "$")


def glob_matches(path: str, pattern: str) -> bool:
    return glob_regex(pattern).match(path) is not None


def is_low_path(path: str) -> bool:
    return any(glob_matches(path, pattern) for pattern in LOW_PATH_GLOBS)


def item_paths(item: dict[str, Any]) -> tuple[str, ...]:
    current = normalize_path(item["filename"])
    if item.get("status") != "renamed":
        return (current,)
    previous = normalize_path(item.get("previous_filename"))
    return (current, previous)


def validate_files(files: Any, expected_count: int) -> list[dict[str, Any]]:
    require(0 < expected_count <= MAX_PR_FILES, "PR file count exceeds the complete API boundary")
    require(isinstance(files, list) and len(files) == expected_count, "PR file list is incomplete")
    result: list[dict[str, Any]] = []
    for item in files:
        require(isinstance(item, dict), "PR file record is malformed")
        filename = item.get("filename")
        status = item.get("status")
        require(isinstance(filename, str) and filename, "PR filename is missing")
        require(isinstance(status, str) and status in {"added", "modified", "removed", "renamed", "copied"}, "PR file status is malformed")
        normalize_path(filename)
        if status == "renamed":
            normalize_path(item.get("previous_filename"))
        for key in ("additions", "deletions", "changes"):
            require(isinstance(item.get(key), int) and item[key] >= 0, "PR file counts are malformed")
        if "patch" in item:
            require(item["patch"] is None or isinstance(item["patch"], str), "PR patch is malformed")
        result.append(item)
    return result


def changed_lines(patch: str) -> tuple[list[int], list[int], bool]:
    old_changed, new_changed, saw_hunk = changed_line_content(patch)
    return [line for line, _ in old_changed], [line for line, _ in new_changed], saw_hunk


def changed_line_content(patch: str) -> tuple[list[tuple[int, str]], list[tuple[int, str]], bool]:
    old_line = new_line = 0
    old_changed: list[tuple[int, str]] = []
    new_changed: list[tuple[int, str]] = []
    saw_hunk = False
    for line in patch.splitlines():
        match = HUNK_RE.match(line)
        if match:
            old_line = int(match.group(1))
            new_line = int(match.group(3))
            saw_hunk = True
            continue
        if not saw_hunk or line.startswith("\\"):
            continue
        if line.startswith("+") and not line.startswith("+++"):
            new_changed.append((new_line, line[1:]))
            new_line += 1
        elif line.startswith("-") and not line.startswith("---"):
            old_changed.append((old_line, line[1:]))
            old_line += 1
        else:
            old_line += 1
            new_line += 1
    return old_changed, new_changed, saw_hunk


def source_text(payload: Any) -> str:
    require(isinstance(payload, dict), "source response is malformed")
    encoding = payload.get("encoding")
    content = payload.get("content")
    require(encoding == "base64" and isinstance(content, str), "source response is incomplete")
    try:
        return base64.b64decode("".join(content.split()), validate=True).decode("utf-8")
    except (ValueError, UnicodeDecodeError) as exc:
        raise PolicyError("source response is not valid UTF-8") from exc


def rust_structural_text(source: str) -> str:
    """Blank Rust comments and literals while preserving code positions."""

    output = list(source)

    def blank(start: int, end: int) -> None:
        for position in range(start, end):
            if output[position] not in {"\n", "\r"}:
                output[position] = " "

    index = 0
    while index < len(source):
        if source.startswith("//", index):
            end = source.find("\n", index + 2)
            end = len(source) if end == -1 else end
            blank(index, end)
            index = end
            continue

        if source.startswith("/*", index):
            depth = 1
            end = index + 2
            while end < len(source) and depth:
                if source.startswith("/*", end):
                    depth += 1
                    end += 2
                elif source.startswith("*/", end):
                    depth -= 1
                    end += 2
                else:
                    end += 1
            require(depth == 0, "Rust source has an unterminated block comment")
            blank(index, end)
            index = end
            continue

        raw = RAW_STRING_START_RE.match(source, index)
        if raw and (index == 0 or not (source[index - 1].isalnum() or source[index - 1] == "_")):
            terminator = '"' + raw.group("hashes")
            end = source.find(terminator, raw.end())
            require(end != -1, "Rust source has an unterminated raw string")
            end += len(terminator)
            blank(index, end)
            index = end
            continue

        if source[index] == '"':
            end = index + 1
            escaped = False
            while end < len(source):
                character = source[end]
                if escaped:
                    escaped = False
                elif character == "\\":
                    escaped = True
                elif character == '"':
                    end += 1
                    break
                end += 1
            require(end <= len(source) and source[end - 1] == '"', "Rust source has an unterminated string")
            blank(index, end)
            index = end
            continue

        if source[index] == "'":
            character = CHAR_LITERAL_RE.match(source, index)
            if character:
                blank(index, character.end())
                index = character.end()
                continue

        index += 1
    return "".join(output)


def cfg_test_ranges(source: str) -> list[tuple[int, int]]:
    lines = source.splitlines()
    structural_lines = rust_structural_text(source).splitlines()
    ranges: list[tuple[int, int]] = []
    for index, line in enumerate(lines):
        if not CFG_TEST_RE.match(line):
            continue
        next_index = index + 1
        while next_index < len(lines) and not lines[next_index].strip():
            next_index += 1
        require(next_index < len(lines), "cfg(test) item is incomplete")
        if "{" not in structural_lines[next_index]:
            end = next_index
            while end < len(lines) and ";" not in structural_lines[end]:
                end += 1
            require(end < len(lines), "cfg(test) item has no end")
            ranges.append((index + 1, end + 1))
            continue

        depth = 0
        opened = False
        end = next_index
        for line_index in range(next_index, len(lines)):
            for character in structural_lines[line_index]:
                if character == "{":
                    depth += 1
                    opened = True
                elif character == "}" and opened:
                    depth -= 1
            if opened and depth == 0:
                end = line_index
                break
        require(opened and depth == 0, "cfg(test) item has unbalanced braces")
        ranges.append((index + 1, end + 1))
    return ranges


def line_in_ranges(line_number: int, ranges: Iterable[tuple[int, int]]) -> bool:
    return any(start <= line_number <= end for start, end in ranges)


def strict_test_only_proof(files: list[dict[str, Any]], high_globs: tuple[str, ...], api: Any, base_sha: str, head_sha: str) -> tuple[bool, str]:
    rust_files = [item for item in files if normalize_path(item["filename"]).endswith(RUST_SUFFIX) and is_high_path(normalize_path(item["filename"]), high_globs)]
    if not rust_files:
        return False, "no canonical high-risk Rust file"
    if any(item["status"] != "modified" for item in rust_files):
        return False, "Rust test-only proof requires an unchanged file path"
    for item in files:
        paths = item_paths(item)
        if paths[0] in {normalize_path(rust["filename"]) for rust in rust_files}:
            continue
        if not all(is_low_path(path) for path in paths):
            return False, "non-test or non-low file is present"

    for item in rust_files:
        patch = item.get("patch")
        if not isinstance(patch, str) or patch.endswith("..."):
            return False, "complete Rust patch is unavailable"
        old_changed, new_changed, saw_hunk = changed_lines(patch)
        if not saw_hunk:
            return False, "Rust patch has no complete hunks"
        additions = sum(1 for line in patch.splitlines() if line.startswith("+") and not line.startswith("+++"))
        deletions = sum(1 for line in patch.splitlines() if line.startswith("-") and not line.startswith("---"))
        if not old_changed and not new_changed:
            return False, "Rust patch has no changed lines"
        if additions != item["additions"] or deletions != item["deletions"] or item["changes"] != additions + deletions:
            return False, "Rust patch is truncated"
        changed_text = [line[1:] for line in patch.splitlines() if (line.startswith("+") and not line.startswith("+++")) or (line.startswith("-") and not line.startswith("---"))]
        if any(CFG_BOUNDARY_RE.search(line) for line in changed_text):
            return False, "conditional-compilation boundary changed"

        current_path = normalize_path(item["filename"])
        previous_path = current_path
        base_source = source_text(api.get_source(previous_path, base_sha))
        head_source = source_text(api.get_source(current_path, head_sha))
        old_content, new_content, _ = changed_line_content(patch)
        base_lines = base_source.splitlines()
        head_lines = head_source.splitlines()
        for line_number, content in old_content:
            require(1 <= line_number <= len(base_lines) and base_lines[line_number - 1] == content, "Rust patch does not match base source")
        for line_number, content in new_content:
            require(1 <= line_number <= len(head_lines) and head_lines[line_number - 1] == content, "Rust patch does not match head source")
        if not all(line_in_ranges(line, cfg_test_ranges(base_source)) for line in old_changed):
            return False, "deleted Rust line is outside an existing cfg(test) item"
        if not all(line_in_ranges(line, cfg_test_ranges(head_source)) for line in new_changed):
            return False, "added Rust line is outside an existing cfg(test) item"
    return True, "complete diff is confined to existing cfg(test) items"


def classify(files: list[dict[str, Any]], high_globs: tuple[str, ...], api: Any, base_sha: str, head_sha: str) -> dict[str, Any]:
    evidence: list[dict[str, Any]] = []
    high_matches = []
    for item in files:
        for filename in item_paths(item):
            patterns = is_high_path(filename, high_globs)
            if patterns:
                high_matches.append(filename)
                evidence.append({"path": filename, "high_globs": patterns})
    if high_matches:
        exception, detail = strict_test_only_proof(files, high_globs, api, base_sha, head_sha)
        return {
            "proposed_risk": "risk:medium" if exception else HIGH_LABEL,
            "matching_evidence": evidence,
            "exception_9530": {"applied": exception, "detail": detail},
        }
    if all(all(is_low_path(path) for path in item_paths(item)) for item in files):
        return {"proposed_risk": "risk:low", "matching_evidence": [], "exception_9530": {"applied": False, "detail": "not applicable"}}
    return {"proposed_risk": "risk:medium", "matching_evidence": [], "exception_9530": {"applied": False, "detail": "not applicable"}}


def approval_state(reviews: list[dict[str, Any]], roster: set[str], head_sha: str) -> dict[str, Any]:
    latest = latest_decisive_reviews(reviews)
    current: list[str] = []
    latest_states: dict[str, str] = {}
    for reviewer, review in sorted(latest.items()):
        state = str(review["state"]).upper()
        latest_states[reviewer] = state
        if reviewer in roster and state == "APPROVED" and review.get("commit_id") == head_sha:
            current.append(reviewer)
    return {
        "required": 2,
        "current_head_core_approvals": sorted(current),
        "count": len(current),
        "latest_decisive_states": latest_states,
        "passed": len(current) >= 2,
    }


def build_report(
    pr: dict[str, Any],
    reviews: list[dict[str, Any]],
    roster: set[str],
    classification: dict[str, Any],
) -> dict[str, Any]:
    head_sha, _, live_labels, _ = parse_pr_metadata(pr)
    approvals = approval_state(reviews, roster, head_sha)
    manual = MANUAL_LABEL in live_labels
    security = SECURITY_LABEL in live_labels
    triggered = HIGH_LABEL in live_labels or security
    current_risk = sorted(live_labels & {"risk:low", "risk:medium", HIGH_LABEL})
    mismatches: list[str] = []
    if len(current_risk) > 1:
        mismatches.append("multiple current risk labels")
    if current_risk and classification["proposed_risk"] not in current_risk and not manual:
        mismatches.append("proposed risk differs from current risk label")
    if manual and current_risk and classification["proposed_risk"] not in current_risk:
        mismatches.append("risk:manual freezes the proposed replacement")
    return {
        "head_sha": head_sha,
        "proposed_risk": classification["proposed_risk"],
        "matching_evidence": classification["matching_evidence"],
        "current_risk": current_risk,
        "risk_manual": manual,
        "mutation_freeze": manual,
        "domain_security": security,
        "approval_gate": {"triggered": triggered, **approvals},
        "mismatches": mismatches,
        "exception_9530": classification["exception_9530"],
    }


class GitHubAPI:
    """Small read/write GitHub REST client used by the trusted workflow."""

    def __init__(self, repository: str, token: str, api_url: str | None = None) -> None:
        require(repository and token, "GitHub API credentials are missing")
        self.repository = repository
        self.base_url = (api_url or os.environ.get("GITHUB_API_URL", "https://api.github.com")).rstrip("/")
        self.token = token

    def request(self, method: str, path: str, payload: dict[str, Any] | None = None) -> Any:
        url = urljoin(self.base_url + "/", path.lstrip("/"))
        body = json.dumps(payload).encode("utf-8") if payload is not None else None
        request = Request(
            url,
            data=body,
            method=method,
            headers={
                "Accept": "application/vnd.github+json",
                "Authorization": f"Bearer {self.token}",
                "User-Agent": "zeroclaw-pr-risk-policy",
                **({"Content-Type": "application/json"} if body is not None else {}),
            },
        )
        try:
            with urlopen(request, timeout=30) as response:
                raw = response.read()
        except (HTTPError, URLError, TimeoutError) as exc:
            raise PolicyError("GitHub API request failed") from exc
        try:
            return json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, json.JSONDecodeError) as exc:
            raise PolicyError("GitHub API returned invalid JSON") from exc

    def get_pull(self, number: int) -> Any:
        return self.request("GET", f"/repos/{self.repository}/pulls/{number}")

    def paginate(self, path: str) -> list[Any]:
        values: list[Any] = []
        for page in range(1, MAX_PAGES + 1):
            separator = "&" if "?" in path else "?"
            payload = self.request("GET", f"{path}{separator}{urlencode({'per_page': PAGE_SIZE, 'page': page})}")
            require(isinstance(payload, list), "GitHub paginated response is malformed")
            values.extend(payload)
            if len(payload) < PAGE_SIZE:
                return values
        raise PolicyError("GitHub pagination did not terminate")

    def get_source(self, path: str, revision: str) -> Any:
        encoded = "/".join(quote(part, safe="") for part in path.split("/"))
        return self.request("GET", f"/repos/{self.repository}/contents/{encoded}?{urlencode({'ref': revision})}")

    def create_status(self, sha: str, state: str, description: str, context: str) -> Any:
        require(state in {"success", "failure", "error", "pending"}, "status state is invalid")
        return self.request(
            "POST",
            f"/repos/{self.repository}/statuses/{sha}",
            {"state": state, "context": context, "description": description[:140]},
        )


def evaluate(api: Any, pr_number: int, policy_path: Path, roster_path: Path) -> dict[str, Any]:
    high_globs = load_policy(policy_path)
    roster = load_core_roster(roster_path)
    pr = api.get_pull(pr_number)
    head_sha, base_sha, live_labels, changed_file_count = parse_pr_metadata(pr)
    files = validate_files(api.paginate(f"/repos/{api.repository}/pulls/{pr_number}/files"), changed_file_count)
    classification = classify(files, high_globs, api, base_sha, head_sha)
    reviews = validate_reviews(api.paginate(f"/repos/{api.repository}/pulls/{pr_number}/reviews"))
    latest_pr = api.get_pull(pr_number)
    latest_head, latest_base, latest_labels, latest_file_count = parse_pr_metadata(latest_pr)
    require(
        (latest_head, latest_base, latest_labels, latest_file_count) == (head_sha, base_sha, live_labels, changed_file_count),
        "PR metadata changed during evaluation",
    )
    latest_reviews = validate_reviews(api.paginate(f"/repos/{api.repository}/pulls/{pr_number}/reviews"))
    require(review_snapshot(latest_reviews) == review_snapshot(reviews), "PR reviews changed during evaluation")
    report = build_report(latest_pr, latest_reviews, roster, classification)
    report["status"] = "success" if not report["approval_gate"]["triggered"] or report["approval_gate"]["passed"] else "failure"
    report["status_description"] = (
        "Risk policy passed" if report["status"] == "success" else f"Two distinct exact-head Core approvals required ({report['approval_gate']['count']}/2)"
    )
    require(report["head_sha"] == head_sha, "PR head changed during evaluation")
    return report


def write_summary(path: Path, report: dict[str, Any]) -> None:
    serialized = html.escape(json.dumps(report, indent=2, sort_keys=True, ensure_ascii=True))
    path.write_text(f"## PR risk policy report\n\n<pre>{serialized}</pre>\n", encoding="utf-8")


def known_event_head() -> str | None:
    value = os.environ.get("PR_HEAD_SHA")
    return value if value and re.fullmatch(r"[0-9a-fA-F]{40}", value) else None


def main(argv: list[str] | None = None, api: Any | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--pr-number", type=int, required=True)
    parser.add_argument("--policy", type=Path, required=True)
    parser.add_argument("--roster", type=Path, required=True)
    parser.add_argument("--repository", default=DEFAULT_REPOSITORY)
    parser.add_argument("--status-context", default=DEFAULT_STATUS_CONTEXT)
    parser.add_argument("--summary", type=Path)
    parser.add_argument("--allow-policy-failure", action="store_true")
    args = parser.parse_args(argv)
    require(args.pr_number > 0, "PR number is invalid")
    client = api or GitHubAPI(args.repository, os.environ.get("GH_TOKEN", ""))
    known_head = known_event_head()
    try:
        if known_head:
            client.create_status(known_head, "pending", "Risk policy evaluating", args.status_context)
        report = evaluate(client, args.pr_number, args.policy, args.roster)
        if args.summary:
            write_summary(args.summary, report)
        client.create_status(report["head_sha"], report["status"], report["status_description"], args.status_context)
        print(json.dumps(report, sort_keys=True, ensure_ascii=True))
        return 0 if report["status"] == "success" or args.allow_policy_failure else 1
    except Exception:  # Convert every evaluation failure to a bounded error status.
        head = known_head
        if head is None:
            try:
                metadata = client.get_pull(args.pr_number)
                candidate = metadata.get("head", {}).get("sha") if isinstance(metadata, dict) else None
                head = candidate if isinstance(candidate, str) and re.fullmatch(r"[0-9a-fA-F]{40}", candidate) else None
            except Exception:
                head = None
        report = {"status": "error", "status_description": "Risk policy evaluation failed", "error": "evaluation failed"}
        if head:
            try:
                client.create_status(head, "error", report["status_description"], args.status_context)
            except Exception:
                pass
        if args.summary:
            try:
                write_summary(args.summary, report)
            except OSError:
                pass
        print(json.dumps(report, sort_keys=True, ensure_ascii=True), file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
