#!/usr/bin/env python3
"""Fixture-driven tests for the report-only pull-request review queues."""

from __future__ import annotations

import json
import subprocess
import unittest
from datetime import datetime, timezone
from contextlib import redirect_stderr
from io import StringIO
from unittest.mock import patch

try:
    from scripts.github import pr_review_queue as queue
except ModuleNotFoundError:
    import pr_review_queue as queue


NOW = datetime(2026, 8, 29, tzinfo=timezone.utc)
CORE = {"JordanTheJet", "Audacity88", "Nillth", "tidux"}


def pr(number: int = 1, labels: list[str] | None = None, **extra: object) -> dict[str, object]:
    payload: dict[str, object] = {
        "number": number,
        "title": f"Change {number}",
        "author": {"login": "contributor"},
        "state": "OPEN",
        "isDraft": False,
        "labels": [{"name": label} for label in labels or []],
        "url": f"https://github.com/zeroclaw-labs/zeroclaw/pull/{number}",
        "mergeable": "MERGEABLE",
        "mergeStateStatus": "CLEAN",
        "reviewDecision": "REVIEW_REQUIRED",
        "headRefOid": "head",
        "statusCheckRollup": [{"name": "Required Gate", "status": "COMPLETED", "conclusion": "SUCCESS"}],
    }
    payload.update(extra)
    return payload


def review(login: str, state: str, submitted_at: str, commit_id: str = "head", review_id: int = 1) -> dict[str, object]:
    return {
        "id": review_id,
        "user": {"login": login},
        "state": state,
        "submitted_at": submitted_at,
        "commit_id": commit_id,
    }


def labeled(name: str, created_at: str, actor: str = "bot") -> dict[str, object]:
    return {"event": "labeled", "label": {"name": name}, "created_at": created_at, "actor": {"login": actor}}


class ReviewQueueTest(unittest.TestCase):
    def row(self, payload: dict[str, object], lane: str, reviews: list[dict[str, object]] | None = None, timeline: list[dict[str, object]] | None = None) -> dict[str, object]:
        return queue.build_row(lane, payload, reviews or [], timeline or [], CORE, NOW, 7)

    def test_maintainer_eligibility_excludes_blocked_author_action_and_stacked(self) -> None:
        candidate = pr(labels=["needs-maintainer-review"])
        row = self.row(candidate, "maintainer", timeline=[labeled("needs-maintainer-review", "2026-08-20T00:00:00Z")])
        self.assertEqual(row["eligibility"], "eligible")
        for labels in (["needs-maintainer-review", "status:blocked"], ["needs-maintainer-review", "needs-author-action"], ["needs-maintainer-review", "stacked"]):
            self.assertFalse(queue.queue_candidates("maintainer", pr(labels=labels), None))

    def test_core_approval_count_reduces_to_latest_current_state(self) -> None:
        payload = pr(labels=["needs-maintainer-review", "risk:high"])
        reviews = [
            review("Audacity88", "APPROVED", "2026-08-20T00:00:00Z", review_id=1),
            review("Audacity88", "CHANGES_REQUESTED", "2026-08-21T00:00:00Z", review_id=2),
            review("JordanTheJet", "APPROVED", "2026-08-22T00:00:00Z", review_id=3),
        ]
        decision, count, approvers, note = queue.review_facts(payload, reviews, CORE)
        self.assertEqual(decision, "REVIEW_REQUIRED")
        self.assertEqual(count, 1)
        self.assertEqual(approvers, ["JordanTheJet"])
        self.assertIsNone(note)

    def test_native_review_decision_is_preferred_for_row_fact(self) -> None:
        payload = pr(reviewDecision="APPROVED")
        decision, count, _, _ = queue.review_facts(
            payload,
            [review("Audacity88", "CHANGES_REQUESTED", "2026-08-22T00:00:00Z")],
            CORE,
        )
        self.assertEqual(decision, "APPROVED")
        self.assertEqual(count, 0)

    def test_second_core_only_reports_fewer_than_two_current_core_approvals(self) -> None:
        payload = pr(labels=["needs-maintainer-review", "risk:high"], reviewDecision="APPROVED")
        one = [review("Audacity88", "APPROVED", "2026-08-20T00:00:00Z")]
        two = one + [review("JordanTheJet", "APPROVED", "2026-08-21T00:00:00Z", review_id=2)]
        self.assertEqual(len(queue.collect_rows("second-core", [payload], 7, None, NOW, lambda *args: [], CORE)), 0)
        fake_gh = lambda *args: []
        with patch.object(queue, "fetch_pr_details", return_value=(one, [])):
            self.assertEqual(len(queue.collect_rows("second-core", [payload], 7, None, NOW, fake_gh, CORE)), 1)
        with patch.object(queue, "fetch_pr_details", return_value=(two, [])):
            self.assertEqual(len(queue.collect_rows("second-core", [payload], 7, None, NOW, fake_gh, CORE)), 0)

    def test_blocked_merge_state_does_not_override_mergeable_fact(self) -> None:
        payload = pr(labels=["needs-maintainer-review"], mergeStateStatus="BLOCKED", mergeable="MERGEABLE")
        row = self.row(payload, "maintainer", timeline=[labeled("needs-maintainer-review", "2026-08-20T00:00:00Z")])
        self.assertEqual(row["mergeability"], "MERGEABLE")
        self.assertEqual(row["eligibility"], "eligible")

    def test_conflicting_mergeability_is_ineligible(self) -> None:
        payload = pr(labels=["needs-maintainer-review"], mergeable="CONFLICTING")
        row = self.row(payload, "maintainer", timeline=[labeled("needs-maintainer-review", "2026-08-20T00:00:00Z")])
        self.assertEqual(row["eligibility"], "ineligible")
        self.assertIn("mergeability is conflicting", row["note"])

    def test_failed_or_non_success_gate_is_ineligible(self) -> None:
        for conclusion, expected in (("FAILURE", "FAILURE"), ("SKIPPED", "UNKNOWN"), ("NEUTRAL", "UNKNOWN")):
            payload = pr(
                labels=["needs-maintainer-review"],
                statusCheckRollup=[{"name": "Required Gate", "status": "COMPLETED", "conclusion": conclusion}],
            )
            row = self.row(payload, "maintainer", timeline=[labeled("needs-maintainer-review", "2026-08-20T00:00:00Z")])
            self.assertEqual(row["required_gate"], expected)
            self.assertEqual(row["eligibility"], "ineligible" if expected == "FAILURE" else "unknown")

    def test_stale_core_approval_has_unknown_applicability(self) -> None:
        payload = pr(labels=["needs-maintainer-review", "risk:high"])
        decision, count, approvers, note = queue.review_facts(
            payload,
            [review("Audacity88", "APPROVED", "2026-08-20T00:00:00Z", commit_id="older-head")],
            CORE,
        )
        self.assertEqual(decision, "REVIEW_REQUIRED")
        self.assertEqual(count, 0)
        self.assertEqual(approvers, [])
        self.assertIn("older head", note or "")

    def test_current_core_approval_remains_eligible_with_stale_diagnostic(self) -> None:
        payload = pr(labels=["needs-maintainer-review", "risk:high"], reviewDecision="APPROVED")
        reviews = [
            review("Audacity88", "APPROVED", "2026-08-20T00:00:00Z"),
            review("Nillth", "APPROVED", "2026-08-21T00:00:00Z", commit_id="older-head", review_id=2),
        ]
        timeline = [labeled("needs-maintainer-review", "2026-08-20T00:00:00Z")]
        with patch.object(queue, "fetch_pr_details", return_value=(reviews, timeline)):
            rows = queue.collect_rows("second-core", [payload], 7, None, NOW, lambda *args: [], CORE)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["eligibility"], "eligible")
        self.assertEqual(rows[0]["core_approvals"], 1)
        self.assertIn("older head", rows[0]["note"])


    def test_two_current_core_approvals_exclude_second_core_despite_stale_diagnostic(self) -> None:
        payload = pr(labels=["needs-maintainer-review", "risk:high"], reviewDecision="APPROVED")
        reviews = [
            review("Audacity88", "APPROVED", "2026-08-20T00:00:00Z"),
            review("JordanTheJet", "APPROVED", "2026-08-21T00:00:00Z", review_id=2),
            review("Nillth", "APPROVED", "2026-08-22T00:00:00Z", commit_id="older-head", review_id=3),
        ]
        decision, count, approvers, note = queue.review_facts(payload, reviews, CORE)
        self.assertEqual(decision, "APPROVED")
        self.assertEqual(count, 2)
        self.assertEqual(approvers, ["Audacity88", "JordanTheJet"])
        self.assertIn("older head", note or "")
        with patch.object(queue, "fetch_pr_details", return_value=(reviews, [labeled("needs-maintainer-review", "2026-08-20T00:00:00Z")])):
            self.assertEqual(len(queue.collect_rows("second-core", [payload], 7, None, NOW, lambda *args: [], CORE)), 0)

    def test_mine_author_matching_is_case_insensitive(self) -> None:
        payload = pr(labels=["needs-maintainer-review"], author={"login": "Audacity88"})
        self.assertTrue(queue.queue_candidates("mine", payload, "audacity88"))
        self.assertTrue(queue.queue_candidates("mine", payload, "AuDaCiTy88"))

    def test_mixed_case_reviewer_logins_reduce_to_one_current_state(self) -> None:
        states = queue.current_review_states(
            [
                review("Audacity88", "APPROVED", "2026-08-20T00:00:00Z"),
                review("audacity88", "CHANGES_REQUESTED", "2026-08-21T00:00:00Z", review_id=2),
            ]
        )
        self.assertEqual(len(states), 1)
        self.assertEqual(next(iter(states.values()))["state"], "CHANGES_REQUESTED")

    def test_second_core_excludes_non_core_only_approval(self) -> None:
        payload = pr(labels=["needs-maintainer-review", "risk:high"])
        reviews = [review("community-reviewer", "APPROVED", "2026-08-20T00:00:00Z")]
        with patch.object(queue, "fetch_pr_details", return_value=(reviews, [])):
            self.assertEqual(len(queue.collect_rows("second-core", [payload], 7, None, NOW, lambda *args: [], CORE)), 0)

    def test_second_core_requires_native_approved_decision(self) -> None:
        payload = pr(labels=["needs-maintainer-review", "risk:high"], reviewDecision="CHANGES_REQUESTED")
        reviews = [review("Audacity88", "APPROVED", "2026-08-20T00:00:00Z")]
        with patch.object(queue, "fetch_pr_details", return_value=(reviews, [labeled("needs-maintainer-review", "2026-08-20T00:00:00Z")])):
            self.assertEqual(len(queue.collect_rows("second-core", [payload], 7, None, NOW, lambda *args: [], CORE)), 0)

    def test_second_core_missing_wait_start_is_unknown(self) -> None:
        payload = pr(labels=["needs-maintainer-review", "risk:high"], reviewDecision="APPROVED")
        reviews = [review("Audacity88", "APPROVED", "2026-08-20T00:00:00Z")]
        with patch.object(queue, "fetch_pr_details", return_value=(reviews, [])):
            rows = queue.collect_rows("second-core", [payload], 7, None, NOW, lambda *args: [], CORE)
        self.assertEqual(len(rows), 1)
        self.assertEqual(rows[0]["eligibility"], "unknown")
        self.assertIn("needs-maintainer-review start", rows[0]["note"])

    def test_author_action_clock_ignores_bot_metadata_churn(self) -> None:
        payload = pr(labels=["needs-author-action"])
        timeline = [
            labeled("needs-author-action", "2026-08-10T00:00:00Z"),
            labeled("size:M", "2026-08-27T00:00:00Z"),
            {"event": "commented", "created_at": "2026-08-28T00:00:00Z", "actor": {"login": "zeroclaw-bot"}},
        ]
        row = self.row(payload, "author-action", timeline=timeline)
        self.assertEqual(row["wait_start"], "2026-08-10T00:00:00Z")
        self.assertEqual(row["wait_days"], 19.0)

    def test_light_lane_preserves_discovery_facts_and_marks_gate_not_checked(self) -> None:
        for lane, label in (("author-action", "needs-author-action"), ("stacked", "stacked")):
            payload = pr(labels=[label], mergeable="CONFLICTING", reviewDecision="CHANGES_REQUESTED")
            payload.pop("statusCheckRollup")
            payload.pop("headRefOid")
            timeline = [labeled(label, "2026-08-10T00:00:00Z")]
            row = self.row(payload, lane, timeline=timeline)
            self.assertEqual(row["mergeability"], "CONFLICTING")
            self.assertEqual(row["review_decision"], "CHANGES_REQUESTED")
            self.assertEqual(row["required_gate"], "NOT_CHECKED")

    def test_repeated_bot_label_event_does_not_reset_clock(self) -> None:
        payload = pr(labels=["needs-author-action"])
        timeline = [
            labeled("needs-author-action", "2026-08-10T00:00:00Z"),
            labeled("needs-author-action", "2026-08-20T00:00:00Z"),
        ]
        row = self.row(payload, "author-action", timeline=timeline)
        self.assertEqual(row["wait_start"], "2026-08-10T00:00:00Z")

    def test_author_comment_answers_action_and_makes_clock_unknown(self) -> None:
        payload = pr(labels=["needs-author-action"])
        timeline = [
            labeled("needs-author-action", "2026-08-10T00:00:00Z"),
            {"event": "commented", "created_at": "2026-08-11T00:00:00Z", "actor": {"login": "contributor"}},
        ]
        row = self.row(payload, "author-action", timeline=timeline)
        self.assertIsNone(row["wait_start"])
        self.assertEqual(row["eligibility"], "unknown")
        self.assertIn("does not prove every finding", row["note"])

    def test_unknown_gate_mergeability_and_timeline_are_explicit(self) -> None:
        payload = pr(labels=["needs-maintainer-review"], mergeable=None, mergeStateStatus=None, statusCheckRollup=[])
        row = self.row(payload, "maintainer")
        self.assertEqual(row["eligibility"], "unknown")
        self.assertIn("mergeability is unknown", row["note"])
        self.assertIn("Required Gate is unknown", row["note"])

    def test_output_formats_and_search_query_are_report_only(self) -> None:
        rows = [self.row(pr(labels=["needs-maintainer-review"]), "maintainer", timeline=[labeled("needs-maintainer-review", "2026-08-20T00:00:00Z")])]
        table = queue.render_table(rows)
        links = queue.render_links("maintainer", rows, None, 7)
        encoded = queue.search_query("maintainer", None, 7)
        self.assertIn("NUMBER", table)
        self.assertIn("#1 https://github.com/zeroclaw-labs/zeroclaw/pull/1", links)
        self.assertIn("label%3Aneeds-maintainer-review", links)
        self.assertIn("repo:zeroclaw-labs/zeroclaw", encoded)
        self.assertIn("draft:false", encoded)
        self.assertNotIn("is:draft:false", encoded)
        self.assertEqual(json.loads(json.dumps(rows))[0]["queue"], "maintainer")

    def test_all_links_emit_one_valid_search_url_per_lane(self) -> None:
        rows = [
            self.row(pr(1, labels=["needs-maintainer-review"]), "maintainer", timeline=[labeled("needs-maintainer-review", "2026-08-20T00:00:00Z")]),
            self.row(pr(2, labels=["stacked"]), "stacked"),
        ]
        links = queue.render_links("all", rows, None, 7)
        lane_lines = [line for line in links.splitlines() if line.startswith(("maintainer:", "second-core:", "author-action:", "stacked:", "mine:")) and "/pulls?q=" in line]
        self.assertEqual(len(lane_lines), 4)
        self.assertTrue(all("/pulls?q=" in line for line in lane_lines))
        self.assertFalse(any(" OR " in line for line in lane_lines))
        self.assertNotIn("mine: https://", links)

        links_with_author = queue.render_links("all", rows, "Audacity88", 7)
        author_lane_lines = [line for line in links_with_author.splitlines() if line.startswith(("maintainer:", "second-core:", "author-action:", "stacked:", "mine:"))]
        self.assertEqual(len(author_lane_lines), 5)
        self.assertIn("mine:", links_with_author)
        self.assertIn("author%3AAudacity88", links_with_author)
        omitted = queue.render_links("all", rows, None, 7)
        self.assertIn("mine: omitted (pass --author LOGIN to include it)", omitted)

    def test_fetch_details_uses_paginated_read_only_gh_calls(self) -> None:
        calls: list[tuple[str, ...]] = []

        def fake_gh(*args: str) -> list[list[dict[str, object]]]:
            calls.append(args)
            return [[{"id": 1}]]

        reviews, timeline = queue.fetch_pr_details(pr(42), fake_gh)
        self.assertEqual(reviews, [{"id": 1}])
        self.assertEqual(timeline, [{"id": 1}])
        self.assertEqual(len(calls), 2)
        self.assertTrue(all("--paginate" in call and "--slurp" in call for call in calls))

    def test_single_lane_discovery_passes_a_filtered_search(self) -> None:
        calls: list[tuple[str, ...]] = []

        def fake_gh(*args: str) -> list[dict[str, object]]:
            calls.append(args)
            return []

        self.assertEqual(queue.fetch_pull_requests("maintainer", gh=fake_gh), [])
        self.assertEqual(len(calls), 1)
        self.assertIn("--search", calls[0])
        search = calls[0][calls[0].index("--search") + 1]
        self.assertIn("label:needs-maintainer-review", search)
        self.assertNotIn("status:blocked", search.replace("-label:status:blocked", ""))
        self.assertNotIn("--search", calls[0][calls[0].index("--search") + 2 :])

    def test_discovery_field_sets_are_minimal_for_light_lanes(self) -> None:
        def requested_fields(call: tuple[str, ...]) -> str:
            return call[call.index("--json") + 1]

        for lane in ("author-action", "stacked"):
            calls: list[tuple[str, ...]] = []

            def fake_gh(*args: str) -> list[dict[str, object]]:
                calls.append(args)
                return []

            self.assertEqual(queue.fetch_pull_requests(lane, gh=fake_gh), [])
            self.assertEqual(requested_fields(calls[0]), queue.COMMON_DISCOVERY_FIELDS)
            self.assertNotIn("statusCheckRollup", requested_fields(calls[0]))
            self.assertNotIn("headRefOid", requested_fields(calls[0]))

        for lane, author in (("maintainer", None), ("mine", "Audacity88"), ("second-core", None)):
            calls = []

            def fake_gh(*args: str) -> list[dict[str, object]]:
                calls.append(args)
                return []

            self.assertEqual(queue.fetch_pull_requests(lane, author, fake_gh), [])
            self.assertEqual(requested_fields(calls[0]), queue.FULL_DISCOVERY_FIELDS)

    def test_discovery_validation_is_lane_aware_and_fail_closed(self) -> None:
        minimal = pr(10, labels=["needs-author-action"])
        for field in ("statusCheckRollup", "headRefOid"):
            minimal.pop(field)
        self.assertIs(queue.validate_pr(minimal, "author-action response", "author-action"), minimal)
        self.assertIs(queue.validate_pr(minimal, "stacked response", "stacked"), minimal)

        for field in ("mergeable", "reviewDecision"):
            malformed_type = dict(minimal)
            malformed_type[field] = []
            with self.subTest(field=field), self.assertRaisesRegex(queue.GitHubCommandError, f"{field} has unexpected type"):
                queue.validate_pr(malformed_type, "author-action response", "author-action")

        for lane in ("author-action", "stacked"):
            malformed = dict(minimal)
            malformed.pop("title")
            with self.subTest(lane=lane), self.assertRaisesRegex(queue.GitHubCommandError, "missing title"):
                queue.validate_pr(malformed, f"{lane} response", lane)

        malformed_full = pr(11, labels=["needs-maintainer-review"])
        malformed_full.pop("title")
        with self.assertRaisesRegex(queue.GitHubCommandError, "missing title"):
            queue.validate_pr(malformed_full, "maintainer response", "maintainer")

        full_missing = pr(11, labels=["needs-maintainer-review"])
        del full_missing["mergeable"]
        with self.assertRaisesRegex(queue.GitHubCommandError, "missing mergeable"):
            queue.validate_pr(full_missing, "maintainer response", "maintainer")

    def test_all_discovery_fails_closed_on_cross_lane_duplicate(self) -> None:
        maintainer_pr = pr(1, labels=["needs-maintainer-review"])
        duplicate_author_pr = pr(1, labels=["needs-author-action"])
        author_action_pr = pr(2, labels=["needs-author-action"])
        stacked_pr = pr(3, labels=["stacked"])
        for payload in (duplicate_author_pr, author_action_pr, stacked_pr):
            for field in ("url", "mergeStateStatus", "statusCheckRollup", "headRefOid"):
                payload.pop(field, None)
        calls: list[tuple[str, ...]] = []

        def fake_gh(*args: str) -> list[dict[str, object]]:
            calls.append(args)
            search = args[args.index("--search") + 1]
            if "needs-maintainer-review" in search:
                return [maintainer_pr]
            if "needs-author-action" in search:
                return [duplicate_author_pr, author_action_pr]
            return [stacked_pr]

        with self.assertRaisesRegex(queue.GitHubCommandError, "snapshot changed.*rerun"):
            queue.fetch_pull_requests("all", "Audacity88", fake_gh)
        self.assertEqual(len(calls), 2)

    def test_all_discovery_deduplicates_stable_author_action_stacked_overlap_and_collects_both_rows(self) -> None:
        author_action_pr = pr(4, labels=["needs-author-action", "stacked"], reviewDecision="REVIEW_REQUIRED")
        stacked_pr = dict(author_action_pr)
        for field in ("url", "mergeStateStatus", "statusCheckRollup", "headRefOid"):
            stacked_pr.pop(field, None)
        calls: list[tuple[str, ...]] = []

        def fake_gh(*args: str) -> list[dict[str, object]]:
            calls.append(args)
            search = args[args.index("--search") + 1]
            if "needs-maintainer-review" in search:
                return []
            if "needs-author-action" in search:
                return [author_action_pr]
            return [stacked_pr]

        discovered = queue.fetch_pull_requests("all", None, fake_gh)
        self.assertEqual(len(discovered), 1)
        self.assertEqual(discovered[0]["number"], 4)
        timeline = [
            labeled("needs-author-action", "2026-08-10T00:00:00Z"),
            labeled("stacked", "2026-08-10T00:00:00Z"),
        ]
        with patch.object(queue, "fetch_pr_details", return_value=([], timeline)):
            rows = queue.collect_rows("all", discovered, 7, None, NOW, lambda *args: [], CORE)
        self.assertEqual([row["queue"] for row in rows], ["author-action", "stacked"])
        self.assertEqual([row["number"] for row in rows], [4, 4])

    def test_all_discovery_unions_heterogeneous_lanes_without_duplicates(self) -> None:
        maintainer_pr = pr(1, labels=["needs-maintainer-review"])
        author_action_pr = pr(2, labels=["needs-author-action"])
        stacked_pr = pr(3, labels=["stacked"])
        for payload in (author_action_pr, stacked_pr):
            for field in ("url", "mergeStateStatus", "statusCheckRollup", "headRefOid"):
                payload.pop(field, None)
        calls: list[tuple[str, ...]] = []

        def fake_gh(*args: str) -> list[dict[str, object]]:
            calls.append(args)
            search = args[args.index("--search") + 1]
            if "needs-maintainer-review" in search:
                return [maintainer_pr]
            if "needs-author-action" in search:
                return [author_action_pr]
            return [stacked_pr]

        discovered = queue.fetch_pull_requests("all", "Audacity88", fake_gh)
        self.assertEqual([item["number"] for item in discovered], [1, 2, 3])
        self.assertEqual(len(calls), 3)

    def test_malformed_nested_discovery_metadata_fails_closed(self) -> None:
        malformed_labels = pr(11)
        malformed_labels["labels"] = [{"name": "needs-maintainer-review"}, {"name": 17}]
        captured = StringIO()
        with redirect_stderr(captured):
            result = queue.main(["--queue", "maintainer"], gh=lambda *args: [malformed_labels])
        self.assertEqual(result, 1)
        self.assertIn("labels: entry 1", captured.getvalue())

        malformed_author = pr(12, labels=["needs-maintainer-review"], author={"name": "contributor"})
        captured = StringIO()
        with redirect_stderr(captured):
            result = queue.main(["--queue", "maintainer"], gh=lambda *args: [malformed_author])
        self.assertEqual(result, 1)
        self.assertIn("author.login", captured.getvalue())

    def test_older_than_days_requires_finite_non_negative_value(self) -> None:
        options = (
            ["--older-than-days", "nan"],
            ["--older-than-days", "infinity"],
            ["--older-than-days=-infinity"],
            ["--older-than-days", "-1"],
        )
        for option in options:
            with self.subTest(option=option), self.assertRaises(SystemExit), redirect_stderr(StringIO()):
                queue.parse_args(["--queue", "maintainer", *option])

    def test_gh_failure_preserves_stderr(self) -> None:
        failure = subprocess.CalledProcessError(1, ["gh"], stderr="GraphQL HTTP 502")
        with patch.object(queue.subprocess, "run", side_effect=failure) as run:
            with self.assertRaisesRegex(queue.GitHubCommandError, "GraphQL HTTP 502"):
                queue.run_gh("pr", "list")
        self.assertEqual(run.call_count, 2)

        captured = StringIO()
        with redirect_stderr(captured):
            result = queue.main(["--queue", "maintainer"], gh=lambda *args: (_ for _ in ()).throw(queue.GitHubCommandError("gh command failed: GraphQL HTTP 502")))
        self.assertEqual(result, 1)
        self.assertIn("GraphQL HTTP 502", captured.getvalue())

    def test_schema_drift_fails_closed_at_main(self) -> None:
        captured = StringIO()
        with redirect_stderr(captured):
            result = queue.main(["--queue", "maintainer"], gh=lambda *args: {"unexpected": "shape"})
        self.assertEqual(result, 1)
        self.assertIn("expected a JSON list", captured.getvalue())

        malformed = pr(9)
        del malformed["title"]
        captured = StringIO()
        with redirect_stderr(captured):
            result = queue.main(["--queue", "maintainer"], gh=lambda *args: [malformed])
        self.assertEqual(result, 1)
        self.assertIn("missing title", captured.getvalue())

    def test_detail_fetches_are_bounded_and_fail_without_partial_rows(self) -> None:
        payloads = [pr(index, labels=["needs-maintainer-review"]) for index in range(1, 4)]
        calls: list[int] = []

        def fake_details(item: dict[str, object], gh: object) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
            calls.append(int(item["number"]))
            return [], [labeled("needs-maintainer-review", "2026-08-20T00:00:00Z")]

        with patch.object(queue, "fetch_pr_details", side_effect=fake_details):
            rows = queue.collect_rows("maintainer", payloads, 7, None, NOW, lambda *args: [], CORE)
        self.assertEqual(sorted(calls), [1, 2, 3])
        self.assertEqual(len(rows), 3)

        def fail_details(item: dict[str, object], gh: object) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
            raise queue.GitHubCommandError(f"detail failed for {item['number']}")

        with patch.object(queue, "fetch_pr_details", side_effect=fail_details):
            with self.assertRaisesRegex(queue.GitHubCommandError, "detail failed"):
                queue.collect_rows("maintainer", payloads, 7, None, NOW, lambda *args: [], CORE)

    def test_detail_failure_stops_rolling_submission(self) -> None:
        payloads = [pr(index, labels=["needs-maintainer-review"]) for index in range(1, 11)]
        calls: list[int] = []

        def fail_first(item: dict[str, object], gh: object) -> tuple[list[dict[str, object]], list[dict[str, object]]]:
            calls.append(int(item["number"]))
            raise queue.GitHubCommandError(f"detail failed for {item['number']}")

        with patch.object(queue, "MAX_DETAIL_WORKERS", 1), patch.object(queue, "fetch_pr_details", side_effect=fail_first):
            with self.assertRaisesRegex(queue.GitHubCommandError, "detail failed for 1"):
                queue.collect_rows("maintainer", payloads, 7, None, NOW, lambda *args: [], CORE)
        self.assertEqual(calls, [1])

    def test_timeout_is_explicit_and_passes_subprocess_timeout(self) -> None:
        timeout = subprocess.TimeoutExpired(["gh"], queue.GH_TIMEOUT_SECONDS)
        with patch.object(queue.subprocess, "run", side_effect=timeout) as run:
            with self.assertRaisesRegex(queue.GitHubCommandError, "timed out"):
                queue.run_gh("pr", "list")
        self.assertEqual(run.call_count, 2)
        self.assertEqual(run.call_args.kwargs["timeout"], queue.GH_TIMEOUT_SECONDS)

    def test_transient_timeout_retries_once_then_succeeds(self) -> None:
        timeout = subprocess.TimeoutExpired(["gh"], queue.GH_TIMEOUT_SECONDS)
        success = subprocess.CompletedProcess(["gh"], 0, stdout="[]", stderr="")
        with patch.object(queue.subprocess, "run", side_effect=[timeout, success]) as run:
            self.assertEqual(queue.run_gh("pr", "list"), [])
        self.assertEqual(run.call_count, 2)

    def test_transient_502_and_tls_errors_retry_once(self) -> None:
        success = subprocess.CompletedProcess(["gh"], 0, stdout="[]", stderr="")
        for detail in ("HTTP 502: Bad Gateway", "TLS handshake timeout"):
            with self.subTest(detail=detail):
                failure = subprocess.CalledProcessError(1, ["gh"], stderr=detail)
                with patch.object(queue.subprocess, "run", side_effect=[failure, success]) as run:
                    self.assertEqual(queue.run_gh("pr", "list"), [])
                self.assertEqual(run.call_count, 2)

    def test_nontransient_error_is_not_retried(self) -> None:
        failure = subprocess.CalledProcessError(1, ["gh"], stderr="HTTP 401: authentication required")
        with patch.object(queue.subprocess, "run", side_effect=failure) as run:
            with self.assertRaisesRegex(queue.GitHubCommandError, "HTTP 401"):
                queue.run_gh("pr", "list")
        self.assertEqual(run.call_count, 1)

    def test_two_transient_failures_stop_after_two_attempts(self) -> None:
        failures = (
            [subprocess.TimeoutExpired(["gh"], queue.GH_TIMEOUT_SECONDS)] * 2,
            [subprocess.CalledProcessError(1, ["gh"], stderr="HTTP 502: Bad Gateway")] * 2,
            [subprocess.CalledProcessError(1, ["gh"], stderr="TLS handshake timeout")] * 2,
        )
        for pair in failures:
            with self.subTest(error=type(pair[0]).__name__):
                with patch.object(queue.subprocess, "run", side_effect=pair) as run:
                    with self.assertRaises(queue.GitHubCommandError):
                        queue.run_gh("pr", "list")
                self.assertEqual(run.call_count, 2)

    def test_untrusted_text_is_sanitized_and_url_is_canonical(self) -> None:
        payload = pr(
            17,
            labels=["needs-maintainer-review", "risk:high"],
            title="Title\n\t\x1b[31m\u202ehidden",
            author={"login": "Au\rdit"},
            url="https://evil.invalid/pull/999",
            reviewDecision="APPROVED",
        )
        row = self.row(payload, "second-core", [review("Audacity88", "APPROVED", "2026-08-20T00:00:00Z", commit_id="old")])
        self.assertEqual(row["url"], "https://github.com/zeroclaw-labs/zeroclaw/pull/17")
        self.assertNotIn("\n", row["title"])
        self.assertNotIn("\t", row["title"])
        self.assertIn("\\u001b", row["title"])
        self.assertIn("\\u202e", row["title"])
        self.assertIn("\\r", row["author"])
        self.assertIn("\\u0007", row["author"])

    def test_all_without_author_reports_mine_omission(self) -> None:
        captured = StringIO()
        output = StringIO()
        with redirect_stderr(captured), patch("sys.stdout", output):
            result = queue.main(["--queue", "all", "--format", "links"], gh=lambda *args: [])
        self.assertEqual(result, 0)
        self.assertIn("mine queue omitted", captured.getvalue())
        self.assertIn("mine: omitted", output.getvalue())


if __name__ == "__main__":
    unittest.main()
