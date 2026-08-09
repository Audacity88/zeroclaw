#!/usr/bin/env python3
"""Tests for PR size-label classification."""

from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path

import pr_size_label as size_labeler


def change(filename: str, additions: int, deletions: int = 0) -> size_labeler.FileChange:
    return size_labeler.FileChange(filename=filename, additions=additions, deletions=deletions)


class PrSizeLabelTest(unittest.TestCase):
    def test_threshold_boundaries(self) -> None:
        cases = [
            (0, "size:XS"),
            (80, "size:XS"),
            (81, "size:S"),
            (250, "size:S"),
            (251, "size:M"),
            (500, "size:M"),
            (501, "size:L"),
            (1000, "size:L"),
            (1001, "size:XL"),
        ]
        for changed_lines, expected in cases:
            with self.subTest(changed_lines=changed_lines):
                self.assertEqual(size_labeler.select_size_label(changed_lines), expected)

    def test_docs_like_files_do_not_count_toward_effective_size(self) -> None:
        files = [
            change("docs/book/src/maintainers/labels.md", 1000),
            change(".github/ISSUE_TEMPLATE/feature.yml", 1000),
            change(".github/pull_request_template.md", 1000),
            change("README.md", 1000),
            change("crates/zeroclaw-config/src/policy.rs", 10, 5),
        ]
        self.assertEqual(size_labeler.effective_changed_lines(files), 15)

    def test_cargo_lock_does_not_count_toward_effective_size(self) -> None:
        files = [
            change("Cargo.lock", 5000, 2000),
            change("Cargo.toml", 20, 5),
        ]
        self.assertEqual(size_labeler.effective_changed_lines(files), 25)

    def test_plan_adds_first_size_label(self) -> None:
        plan = size_labeler.plan_size_label([change("src/main.rs", 81)], {"bug"})
        self.assertEqual(plan.selected_label, "size:S")
        self.assertEqual(plan.labels_to_add, ("size:S",))
        self.assertEqual(plan.labels_to_remove, ())

    def test_plan_replaces_stale_canonical_size_label(self) -> None:
        plan = size_labeler.plan_size_label(
            [change("src/main.rs", 251)],
            {"size:XS", "risk:low"},
        )
        self.assertEqual(plan.selected_label, "size:M")
        self.assertEqual(plan.labels_to_add, ("size:M",))
        self.assertEqual(plan.labels_to_remove, ("size:XS",))

    def test_plan_removes_extra_canonical_size_labels_without_touching_legacy_spelling(self) -> None:
        plan = size_labeler.plan_size_label(
            [change("src/main.rs", 10)],
            {"size:XS", "size:S", "size: M"},
        )
        self.assertEqual(plan.labels_to_add, ())
        self.assertEqual(plan.labels_to_remove, ("size:S",))

    def test_file_change_rejects_malformed_api_payload(self) -> None:
        with self.assertRaisesRegex(ValueError, "must be an object"):
            size_labeler.file_change_from_api("not-an-object")  # type: ignore[arg-type]
        with self.assertRaisesRegex(ValueError, "invalid additions"):
            size_labeler.file_change_from_api(
                {"filename": "src/main.rs", "additions": -1, "deletions": 0}
            )
        with self.assertRaisesRegex(ValueError, "invalid additions"):
            size_labeler.file_change_from_api(
                {"filename": "src/main.rs", "additions": True, "deletions": 0}
            )
        with self.assertRaisesRegex(ValueError, "invalid deletions"):
            size_labeler.file_change_from_api(
                {"filename": "src/main.rs", "additions": 0, "deletions": False}
            )

    def test_docs_threshold_parser_matches_expected_table(self) -> None:
        docs = "\n".join(
            [
                size_labeler.DOCS_LIKE_CONTRACT_SENTENCE,
                "| Label | Threshold |",
                "|---|---|",
                "| `size:XS` | <= 80 lines |",
                "| `size:S` | <= 250 lines |",
                "| `size:M` | <= 500 lines |",
                "| `size:L` | <= 1000 lines |",
                "| `size:XL` | > 1000 lines |",
            ]
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            path = Path(tmpdir) / "labels.md"
            path.write_text(docs, encoding="utf-8")
            self.assertEqual(size_labeler.docs_thresholds(path), dict(size_labeler.SIZE_THRESHOLDS))
            size_labeler.validate_docs_contract(path)

    def test_docs_contract_rejects_missing_exclusion_sentence(self) -> None:
        docs = "\n".join(
            [
                "| Label | Threshold |",
                "|---|---|",
                "| `size:XS` | <= 80 lines |",
                "| `size:S` | <= 250 lines |",
                "| `size:M` | <= 500 lines |",
                "| `size:L` | <= 1000 lines |",
                "| `size:XL` | > 1000 lines |",
            ]
        )
        with tempfile.TemporaryDirectory() as tmpdir:
            path = Path(tmpdir) / "labels.md"
            path.write_text(docs, encoding="utf-8")
            with self.assertRaisesRegex(ValueError, "exclusion contract"):
                size_labeler.validate_docs_contract(path)

    def test_plan_json_is_stable(self) -> None:
        plan = size_labeler.SizePlan(81, "size:S", ("size:S",), ("size:XS",))
        payload = json.loads(size_labeler.plan_as_json(plan, dry_run=True))
        self.assertEqual(payload["selected_label"], "size:S")
        self.assertTrue(payload["dry_run"])
        self.assertTrue(payload["changed"])


if __name__ == "__main__":
    unittest.main()
