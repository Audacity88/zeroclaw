#!/usr/bin/env python3
"""Focused tests for the report-only pull-request risk policy."""

from __future__ import annotations

import base64
import os
from pathlib import Path
import re
import tempfile
import unittest
from unittest import mock

try:
    from scripts.github import pr_risk_policy as policy
except ModuleNotFoundError:
    import pr_risk_policy as policy


ROOT = Path(__file__).resolve().parents[2]
POLICY_PATH = ROOT / ".github/risk-labeler.yml"
POLICY_WORKFLOW_PATH = ROOT / ".github/workflows/pr-risk-policy.yml"
SIGNAL_WORKFLOW_PATH = ROOT / ".github/workflows/pr-risk-policy-review-signal.yml"
HEAD = "a" * 40
BASE = "b" * 40
SIGNAL_TITLE_RE = re.compile(r"^PR Risk Event Signal pr=([1-9][0-9]*) head=([0-9a-fA-F]{40})$")


def resolve_signal_identity(source_event: str, title: str, native_pr_number: str = "") -> tuple[int, str | None]:
    if source_event == "pull_request_target":
        match = SIGNAL_TITLE_RE.fullmatch(title)
        if match is None:
            raise ValueError("malformed signal identity")
        pr_number = int(match.group(1))
        if native_pr_number and native_pr_number != str(pr_number):
            raise ValueError("signal association mismatch")
        return pr_number, match.group(2)
    if source_event == "pull_request_review":
        if re.fullmatch(r"[1-9][0-9]*", native_pr_number) is None:
            raise ValueError("review signal has no native association")
        return int(native_pr_number), None
    raise ValueError("unsupported signal source")


def pull(labels: list[str] | None = None, **extra: object) -> dict[str, object]:
    value: dict[str, object] = {
        "head": {"sha": HEAD},
        "base": {"sha": BASE},
        "labels": [{"name": label} for label in labels or []],
        "changed_files": 1,
    }
    value.update(extra)
    return value


def changed_file(path: str, additions: int = 1, deletions: int = 0, patch: str | None = None, **extra: object) -> dict[str, object]:
    value: dict[str, object] = {
        "filename": path,
        "status": "modified",
        "additions": additions,
        "deletions": deletions,
        "changes": additions + deletions,
        "patch": patch,
    }
    value.update(extra)
    return value


def review(login: str, state: str = "APPROVED", commit: str = HEAD, review_id: int = 1, submitted_at: str | None = None) -> dict[str, object]:
    return {
        "id": review_id,
        "user": {"login": login},
        "state": state,
        "commit_id": commit,
        "submitted_at": submitted_at or f"2026-08-2{review_id}T00:00:00Z",
    }


class FakeAPI:
    repository = "zeroclaw-labs/zeroclaw"

    def __init__(self, pr: dict[str, object], files: list[dict[str, object]], reviews: list[dict[str, object]] | None = None) -> None:
        self.pr = pr
        self.files = files
        self.pr["changed_files"] = len(files)
        self.reviews = reviews or []
        self.sources: dict[tuple[str, str], dict[str, str]] = {}
        self.statuses: list[tuple[str, str, str, str]] = []

    def get_pull(self, number: int) -> dict[str, object]:
        return self.pr

    def paginate(self, path: str) -> list[dict[str, object]]:
        if path.endswith("/files"):
            return self.files
        if path.endswith("/reviews"):
            return self.reviews
        raise AssertionError(path)

    def get_source(self, path: str, revision: str) -> dict[str, str]:
        return self.sources[(path, revision)]

    def add_source(self, path: str, revision: str, content: str) -> None:
        self.sources[(path, revision)] = {"encoding": "base64", "content": base64.b64encode(content.encode()).decode()}

    def create_status(self, sha: str, state: str, description: str, context: str) -> None:
        self.statuses.append((sha, state, description, context))


def roster_file(directory: Path, text: str | None = None) -> Path:
    path = directory / "communication.md"
    path.write_text(
        text
        or "| Handle | Role | Focus |\n|---|---|---|\n| [@core-one](https://example.invalid/one) | Core Team | Runtime |\n| [@core-two](https://example.invalid/two) | Core Team | Docs |\n",
        encoding="utf-8",
    )
    return path


def evaluate(api: FakeAPI) -> dict[str, object]:
    with tempfile.TemporaryDirectory() as directory:
        return policy.evaluate(api, 1, POLICY_PATH, roster_file(Path(directory)))


TEST_SOURCE = """fn production() {\n    println!(\"production\");\n}\n\n#[cfg(test)]\nmod tests {\n    #[test]\n    fn existing() {\n        assert_eq!(1, 1);\n    }\n}\n"""
TEST_PATCH = """@@ -9,1 +9,1 @@\n-        assert_eq!(1, 1);\n+        assert_eq!(1, 2);\n"""


class RiskPolicyTest(unittest.TestCase):
    def test_policy_uses_accepted_labeler_shape_and_high_globs(self) -> None:
        globs = policy.load_policy(POLICY_PATH)
        self.assertIn("wit/**", globs)
        self.assertIn(".github/workflows/*release*.yml", globs)
        self.assertIn(".github/workflows/pr-risk-policy.yml", globs)
        self.assertIn(".github/workflows/pr-risk-policy-review-signal.yml", globs)
        self.assertIn("docs/book/src/contributing/communication.md", globs)
        self.assertIn("scripts/github/pr_risk_policy.py", globs)
        self.assertIn("crates/zeroclaw-runtime/src/security/**", globs)
        self.assertTrue(policy.glob_matches(".github/workflows/release-stable.yml", ".github/workflows/*release*.yml"))
        self.assertFalse(policy.glob_matches(".github/workflows/nested/release-stable.yml", ".github/workflows/*release*.yml"))

    def test_high_glob_classifies_high_and_reports_evidence(self) -> None:
        api = FakeAPI(pull(), [changed_file("wit/plugin.wit")])
        report = evaluate(api)
        self.assertEqual(report["proposed_risk"], "risk:high")
        self.assertEqual(report["matching_evidence"][0]["path"], "wit/plugin.wit")

    def test_docs_and_fixtures_are_low(self) -> None:
        api = FakeAPI(pull(), [changed_file("docs/book/src/guide.md"), changed_file("tests/fixtures/input.json")])
        report = evaluate(api)
        self.assertEqual(report["proposed_risk"], "risk:low")

    def test_ordinary_behavior_is_medium(self) -> None:
        api = FakeAPI(pull(), [changed_file("crates/zeroclaw-providers/src/openai.rs")])
        report = evaluate(api)
        self.assertEqual(report["proposed_risk"], "risk:medium")

    def test_renaming_out_of_high_boundary_stays_high(self) -> None:
        file = changed_file("crates/zeroclaw-runtime/src/policy.rs", previous_filename="crates/zeroclaw-runtime/src/security/policy.rs", status="renamed")
        api = FakeAPI(pull(), [file])
        report = evaluate(api)
        self.assertEqual(report["proposed_risk"], "risk:high")

    def test_manual_freeze_is_reported_without_suppressing_gate(self) -> None:
        api = FakeAPI(pull(["risk:high", "risk:manual"]), [changed_file("wit/plugin.wit")])
        report = evaluate(api)
        self.assertTrue(report["risk_manual"])
        self.assertTrue(report["approval_gate"]["triggered"])

    def test_security_only_label_triggers_two_approval_gate(self) -> None:
        api = FakeAPI(pull(["domain:security"]), [changed_file("src/providers/openai.rs")], [review("core-one")])
        report = evaluate(api)
        self.assertEqual(report["proposed_risk"], "risk:medium")
        self.assertTrue(report["approval_gate"]["triggered"])
        self.assertFalse(report["approval_gate"]["passed"])

    def test_latest_decisive_exact_head_core_reviews_only(self) -> None:
        reviews = [
            review("core-one", review_id=1),
            review("core-one", state="CHANGES_REQUESTED", review_id=2),
            review("core-two", commit="c" * 40, review_id=3),
            review("automation-bot", review_id=4),
            review("core-three", review_id=5),
            review("core-two", state="PENDING", review_id=6),
        ]
        state = policy.approval_state(policy.validate_reviews(reviews), {"core-one", "core-two"}, HEAD)
        self.assertEqual(state["count"], 0)
        self.assertEqual(state["latest_decisive_states"]["core-one"], "CHANGES_REQUESTED")
        self.assertNotIn("automation-bot", state["current_head_core_approvals"])

        reviews[1] = review("core-one", state="COMMENTED", review_id=2)
        state = policy.approval_state(policy.validate_reviews(reviews), {"core-one", "core-two"}, HEAD)
        self.assertEqual(state["count"], 1)

    def test_two_distinct_exact_head_approvals_pass(self) -> None:
        api = FakeAPI(pull(["risk:high"]), [changed_file("wit/plugin.wit")], [review("core-one"), review("core-two", review_id=2)])
        report = evaluate(api)
        self.assertEqual(report["status"], "success")

    def test_review_change_during_evaluation_fails_closed(self) -> None:
        class ChangingReviewsAPI(FakeAPI):
            review_reads = 0

            def paginate(self, path: str) -> list[dict[str, object]]:
                if not path.endswith("/reviews"):
                    return super().paginate(path)
                self.review_reads += 1
                if self.review_reads == 1:
                    return [review("core-one"), review("core-two", review_id=2)]
                return [review("core-one"), review("core-two", state="DISMISSED", review_id=3)]

        api = ChangingReviewsAPI(pull(["risk:high"]), [changed_file("wit/plugin.wit")])
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(policy.PolicyError, "reviews changed"):
                policy.evaluate(api, 1, POLICY_PATH, roster_file(Path(directory)))

    def test_head_change_during_evaluation_fails_closed(self) -> None:
        class ChangingHeadAPI(FakeAPI):
            pull_reads = 0

            def get_pull(self, number: int) -> dict[str, object]:
                self.pull_reads += 1
                if self.pull_reads == 1:
                    return super().get_pull(number)
                return pull(["risk:high"], head={"sha": "c" * 40})

        api = ChangingHeadAPI(pull(["risk:high"]), [changed_file("wit/plugin.wit")])
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaisesRegex(policy.PolicyError, "metadata changed"):
                policy.evaluate(api, 1, POLICY_PATH, roster_file(Path(directory)))

    def test_malformed_api_and_roster_fail_closed(self) -> None:
        with self.assertRaises(policy.PolicyError):
            policy.parse_pr_metadata({"head": {"sha": HEAD}, "base": {"sha": BASE}, "labels": [{"bad": "label"}], "changed_files": 1})
        with self.assertRaises(policy.PolicyError):
            policy.parse_pr_metadata({"head": {"sha": "not-a-sha"}, "base": {"sha": BASE}, "labels": [], "changed_files": 1})
        with self.assertRaises(policy.PolicyError):
            policy.validate_files([{"filename": "src/lib.rs", "status": "modified"}], 1)
        with tempfile.TemporaryDirectory() as directory:
            path = roster_file(Path(directory), "| Handle | Role |\n|---|---|\n| not-a-link | Core Team |\n")
            with self.assertRaises(policy.PolicyError):
                policy.load_core_roster(path)

            path = roster_file(Path(directory), "| Handle | Role |\n|---|---|\n| [@core-one](https://example.invalid/one) | Core Teamish |\n")
            with self.assertRaises(policy.PolicyError):
                policy.load_core_roster(path)

            path = roster_file(
                Path(directory),
                "| Handle | Role |\n|---|---|\n| [@core-one](https://example.invalid/one) | Core Team |\n| [@CORE-ONE](https://example.invalid/two) | Core Team |\n",
            )
            with self.assertRaises(policy.PolicyError):
                policy.load_core_roster(path)

            path = roster_file(Path(directory), "| Handle | Role |\n|---|---|\n| [@community](https://example.invalid/c) | Community |\n")
            with self.assertRaises(policy.PolicyError):
                policy.load_core_roster(path)

        class IncompleteAPI(FakeAPI):
            def paginate(self, path: str) -> list[dict[str, object]]:
                raise policy.PolicyError("pagination incomplete")

        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(policy.PolicyError):
                policy.evaluate(IncompleteAPI(pull(), [changed_file("docs/book/src/guide.md")]), 1, POLICY_PATH, roster_file(Path(directory)))

        too_many = pull(changed_files=policy.MAX_PR_FILES + 1)
        api = FakeAPI(too_many, [changed_file("docs/book/src/guide.md")])
        api.pr["changed_files"] = policy.MAX_PR_FILES + 1
        with tempfile.TemporaryDirectory() as directory:
            with self.assertRaises(policy.PolicyError):
                policy.evaluate(api, 1, POLICY_PATH, roster_file(Path(directory)))

    def test_missing_or_truncated_patch_stays_high(self) -> None:
        for patch, additions, changes in [(None, 1, 1), (TEST_PATCH, 2, 3)]:
            file = changed_file("crates/zeroclaw-runtime/src/security/policy.rs", additions, 1, patch)
            file["changes"] = changes
            api = FakeAPI(pull(), [file])
            report = evaluate(api)
            self.assertEqual(report["proposed_risk"], "risk:high")
            self.assertFalse(report["exception_9530"]["applied"])

    def test_9530_positive_case_requires_complete_inert_sources(self) -> None:
        file = changed_file("crates/zeroclaw-runtime/src/security/policy.rs", 1, 1, TEST_PATCH)
        api = FakeAPI(pull(), [file])
        api.add_source("crates/zeroclaw-runtime/src/security/policy.rs", BASE, TEST_SOURCE)
        api.add_source("crates/zeroclaw-runtime/src/security/policy.rs", HEAD, TEST_SOURCE.replace("assert_eq!(1, 1)", "assert_eq!(1, 2)"))
        report = evaluate(api)
        self.assertEqual(report["proposed_risk"], "risk:medium")
        self.assertTrue(report["exception_9530"]["applied"])

    def test_9530_negative_cases_remain_high(self) -> None:
        cases = [
            ("@@ -1,1 +1,1 @@\n-fn production() {}\n+fn production() { println!(\"changed\"); }\n", """fn production() {}

#[cfg(test)]
mod tests {
    #[test]
    fn existing() {
        assert_eq!(1, 1);
    }
}
""", """fn production() { println!(\"changed\"); }

#[cfg(test)]
mod tests {
    #[test]
    fn existing() {
        assert_eq!(1, 1);
    }
}
"""),
            ("@@ -9,1 +9,1 @@\n-        assert_eq!(1, 1);\n+        #[cfg(test)]\n", TEST_SOURCE, TEST_SOURCE),
        ]
        for patch, base_source, head_source in cases:
            file = changed_file("crates/zeroclaw-runtime/src/security/policy.rs", 1, 1, patch)
            api = FakeAPI(pull(), [file])
            api.add_source("crates/zeroclaw-runtime/src/security/policy.rs", BASE, base_source)
            api.add_source("crates/zeroclaw-runtime/src/security/policy.rs", HEAD, head_source)
            report = evaluate(api)
            self.assertEqual(report["proposed_risk"], "risk:high")
            self.assertFalse(report["exception_9530"]["applied"])

    def test_9530_lexer_does_not_extend_test_scope_across_literals_or_comments(self) -> None:
        base_source = '''mod outer {
    #[cfg(test)]
    mod tests {
        const OPEN: &str = "{";
        const RAW: &str = r#"}"#;
        const BRACE: char = '{';
        /* } nested /* { */ comment */
    }

    fn production() {
        run_old();
        let _ = "}";
    }
}
'''
        head_source = base_source.replace("run_old();", "run_new();")
        patch = "@@ -11,1 +11,1 @@\n-        run_old();\n+        run_new();\n"
        file = changed_file("crates/zeroclaw-runtime/src/security/policy.rs", 1, 1, patch)
        api = FakeAPI(pull(), [file])
        api.add_source("crates/zeroclaw-runtime/src/security/policy.rs", BASE, base_source)
        api.add_source("crates/zeroclaw-runtime/src/security/policy.rs", HEAD, head_source)
        report = evaluate(api)
        self.assertEqual(report["proposed_risk"], "risk:high")
        self.assertFalse(report["exception_9530"]["applied"])

    def test_main_publishes_one_stable_status(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            roster = roster_file(Path(directory))
            api = FakeAPI(pull(), [changed_file("docs/book/src/guide.md")])
            result = policy.main(
                ["--pr-number", "1", "--policy", str(POLICY_PATH), "--roster", str(roster)],
                api,
            )
        self.assertEqual(result, 0)
        self.assertEqual(api.statuses, [(HEAD, "success", "Risk policy passed", policy.DEFAULT_STATUS_CONTEXT)])

    def test_main_fails_status_for_trigger_without_two_approvals(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            roster = roster_file(Path(directory))
            api = FakeAPI(pull(["risk:high"]), [changed_file("wit/plugin.wit")], [review("core-one")])
            with mock.patch.dict(os.environ, {"PR_HEAD_SHA": HEAD}, clear=False):
                result = policy.main(
                    ["--pr-number", "1", "--policy", str(POLICY_PATH), "--roster", str(roster)],
                    api,
                )
        self.assertEqual(result, 1)
        self.assertEqual(api.statuses[0], (HEAD, "pending", "Risk policy evaluating", policy.DEFAULT_STATUS_CONTEXT))
        self.assertEqual(api.statuses[1][0], HEAD)
        self.assertEqual(api.statuses[1][1], "failure")
        self.assertEqual(api.statuses[1][3], policy.DEFAULT_STATUS_CONTEXT)

    def test_known_event_head_gets_pending_before_success(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            roster = roster_file(Path(directory))
            api = FakeAPI(pull(), [changed_file("docs/book/src/guide.md")])
            with mock.patch.dict(os.environ, {"PR_HEAD_SHA": HEAD}, clear=False):
                result = policy.main(
                    ["--pr-number", "1", "--policy", str(POLICY_PATH), "--roster", str(roster)],
                    api,
                )
        self.assertEqual(result, 0)
        self.assertEqual(
            api.statuses,
            [
                (HEAD, "pending", "Risk policy evaluating", policy.DEFAULT_STATUS_CONTEXT),
                (HEAD, "success", "Risk policy passed", policy.DEFAULT_STATUS_CONTEXT),
            ],
        )

    def test_policy_failure_can_be_reported_without_failing_reconciliation(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            roster = roster_file(Path(directory))
            api = FakeAPI(pull(["risk:high"]), [changed_file("wit/plugin.wit")], [review("core-one")])
            result = policy.main(
                [
                    "--pr-number",
                    "1",
                    "--policy",
                    str(POLICY_PATH),
                    "--roster",
                    str(roster),
                    "--allow-policy-failure",
                ],
                api,
            )
        self.assertEqual(result, 0)
        self.assertEqual(api.statuses[-1][1], "failure")

    def test_error_status_prefers_validated_event_head(self) -> None:
        class BrokenAPI(FakeAPI):
            def get_pull(self, number: int) -> dict[str, object]:
                raise policy.PolicyError("unavailable")

        with tempfile.TemporaryDirectory() as directory:
            roster = roster_file(Path(directory))
            api = BrokenAPI(pull(), [changed_file("docs/book/src/guide.md")])
            with mock.patch.dict(os.environ, {"PR_HEAD_SHA": HEAD}, clear=False):
                result = policy.main(
                    ["--pr-number", "1", "--policy", str(POLICY_PATH), "--roster", str(roster)],
                    api,
                )
        self.assertEqual(result, 1)
        self.assertEqual(
            api.statuses,
            [
                (HEAD, "pending", "Risk policy evaluating", policy.DEFAULT_STATUS_CONTEXT),
                (HEAD, "error", "Risk policy evaluation failed", policy.DEFAULT_STATUS_CONTEXT),
            ],
        )


class WorkflowContractTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.policy_workflow = POLICY_WORKFLOW_PATH.read_text(encoding="utf-8")
        cls.signal_workflow = SIGNAL_WORKFLOW_PATH.read_text(encoding="utf-8")

    def test_privileged_workflow_consumes_only_trusted_events(self) -> None:
        self.assertNotIn("pull_request_target:", self.policy_workflow)
        self.assertNotIn("pull_request_review:", self.policy_workflow)
        self.assertIn("workflow_run:\n    workflows: [PR Risk Event Signal]\n    types: [completed]", self.policy_workflow)
        self.assertIn("push:\n    branches: [master]", self.policy_workflow)
        self.assertIn("workflow_dispatch:", self.policy_workflow)
        self.assertIn(
            "group: pr-risk-policy-${{ needs.resolve.outputs.pr_number }}",
            self.policy_workflow,
        )
        self.assertIn("cancel-in-progress: true", self.policy_workflow)

    def test_signal_covers_pr_updates_and_review_changes_without_permissions(self) -> None:
        self.assertIn("name: PR Risk Event Signal", self.signal_workflow)
        self.assertIn(
            "run-name: PR Risk Event Signal pr=${{ github.event.pull_request.number }} head=${{ github.event.pull_request.head.sha }}",
            self.signal_workflow,
        )
        self.assertIn(
            "pull_request_target:\n    types: [opened, synchronize, reopened, edited, converted_to_draft, ready_for_review, labeled, unlabeled]",
            self.signal_workflow,
        )
        self.assertIn("pull_request_review:\n    types: [submitted, dismissed]", self.signal_workflow)
        self.assertIn("\npermissions: {}\n", self.signal_workflow)
        self.assertNotIn("\n  pull_request:\n", self.signal_workflow)

    def test_workflows_do_not_transfer_or_execute_contributor_content(self) -> None:
        combined = self.policy_workflow + self.signal_workflow
        self.assertNotIn("actions/checkout", combined)
        self.assertNotIn("download-artifact", combined)
        self.assertNotIn("upload-artifact", combined)
        self.assertNotIn("github.event.pull_request.head", self.policy_workflow)
        self.assertNotIn("actions/runs/", self.policy_workflow)

    def test_workflow_run_uses_its_validated_pr_association(self) -> None:
        self.assertIn(
            "WORKFLOW_RUN_EVENT: ${{ github.event.workflow_run.event }}",
            self.policy_workflow,
        )
        self.assertIn(
            "WORKFLOW_RUN_TITLE: ${{ github.event.workflow_run.display_title }}",
            self.policy_workflow,
        )
        self.assertIn(
            "WORKFLOW_RUN_PR_NUMBER: ${{ github.event.workflow_run.pull_requests[0].number }}",
            self.policy_workflow,
        )
        self.assertIn(
            r'[[ ! "$WORKFLOW_RUN_TITLE" =~ ^PR\ Risk\ Event\ Signal\ pr=([1-9][0-9]*)\ head=([0-9a-fA-F]{40})$ ]]',
            self.policy_workflow,
        )
        self.assertIn('pr_number="${BASH_REMATCH[1]}"', self.policy_workflow)
        self.assertIn('signal_head="${BASH_REMATCH[2]}"', self.policy_workflow)
        self.assertIn('pull_request_review)', self.policy_workflow)
        self.assertIn('pr_number="$WORKFLOW_RUN_PR_NUMBER"', self.policy_workflow)
        self.assertIn('[[ "$pr_number" =~ ^[1-9][0-9]*$ ]]', self.policy_workflow)

    def test_target_signal_fixtures_allow_trusted_title_fallback(self) -> None:
        title = f"PR Risk Event Signal pr=42 head={HEAD}"
        fixtures = (
            ("normal", "42"),
            ("fork", "42"),
            ("dependabot", "42"),
            ("empty-association", ""),
        )
        for name, native_association in fixtures:
            with self.subTest(name=name):
                self.assertEqual(resolve_signal_identity("pull_request_target", title, native_association), (42, HEAD))

    def test_review_signal_fixtures_require_native_association_and_ignore_title(self) -> None:
        for name in ("normal", "fork", "dependabot"):
            with self.subTest(name=name):
                self.assertEqual(resolve_signal_identity("pull_request_review", "attacker-controlled", "42"), (42, None))
        with self.assertRaisesRegex(ValueError, "no native association"):
            resolve_signal_identity("pull_request_review", f"PR Risk Event Signal pr=42 head={HEAD}", "")

    def test_signal_identity_rejects_malformed_or_mismatched_values(self) -> None:
        malformed = (
            f"PR Risk Event Signal pr=0 head={HEAD}",
            f"PR Risk Event Signal pr=42 head={HEAD} extra=x",
            "PR Risk Event Signal pr=42 head=not-a-sha",
        )
        for title in malformed:
            with self.subTest(title=title):
                with self.assertRaises(ValueError):
                    resolve_signal_identity("pull_request_target", title)
        with self.assertRaisesRegex(ValueError, "association mismatch"):
            resolve_signal_identity("pull_request_target", f"PR Risk Event Signal pr=42 head={HEAD}", "43")
        with self.assertRaisesRegex(ValueError, "unsupported signal source"):
            resolve_signal_identity("pull_request", f"PR Risk Event Signal pr=42 head={HEAD}", "42")

    def test_live_resolution_failure_uses_only_encoded_head_for_error_status(self) -> None:
        self.assertIn('statuses/$signal_head', self.policy_workflow)
        self.assertIn("Risk policy could not resolve live PR state", self.policy_workflow)
        self.assertIn('head_sha="$(gh api "repos/$REPOSITORY/pulls/$pr_number"', self.policy_workflow)
        review_branch = self.policy_workflow.split("pull_request_review)", 1)[1].split(";;", 1)[0]
        self.assertNotIn("signal_head=", review_branch)

    def test_privileged_permissions_and_status_context_are_fixed(self) -> None:
        self.assertIn(
            "permissions:\n  contents: read\n  pull-requests: read\n  statuses: write\n",
            self.policy_workflow,
        )
        self.assertIn("--status-context 'zeroclaw/pr-risk-policy'", self.policy_workflow)
        self.assertIn("-f context='zeroclaw/pr-risk-policy'", self.policy_workflow)
        self.assertNotIn("actions: read", self.policy_workflow)
        self.assertNotIn("pull_request_target:", self.policy_workflow)

    def test_policy_inputs_come_from_the_default_branch(self) -> None:
        for path in (
            "scripts/github/pr_risk_policy.py",
            ".github/risk-labeler.yml",
            "docs/book/src/contributing/communication.md",
        ):
            self.assertIn(f'fetch_trusted "{path}"', self.policy_workflow)
        self.assertIn('contents/$path?ref=$DEFAULT_BRANCH', self.policy_workflow)

    def test_open_pr_reconciliation_validates_each_live_head(self) -> None:
        self.assertIn('[[ "$head_sha" =~ ^[0-9a-fA-F]{40}$ ]]', self.policy_workflow)
        self.assertIn('--allow-policy-failure', self.policy_workflow)


if __name__ == "__main__":
    unittest.main()
