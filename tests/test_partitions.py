"""Tests for autoresearch.partitions."""
from __future__ import annotations

import pytest

from autoresearch.partitions import (
    CV_FRACTION,
    HOLDOUT_FRACTION,
    TRAIN_FRACTION,
    Partitions,
    make_partitions,
)


class TestMakePartitions:
    def test_default_fractions_sum_to_one(self):
        assert abs(TRAIN_FRACTION + CV_FRACTION + HOLDOUT_FRACTION - 1.0) < 1e-9

    def test_typical_n(self):
        p = make_partitions(1000)
        assert p.train_start == 0
        assert p.train_end == 600
        assert p.cv_start == 600
        assert p.cv_end == 800
        assert p.holdout_start == 800
        assert p.holdout_end == 1000

    def test_partitions_contiguous(self):
        for n in (5, 10, 100, 1000, 9099, 12345):
            p = make_partitions(n)
            assert p.train_end == p.cv_start
            assert p.cv_end == p.holdout_start
            assert p.holdout_end == n

    def test_partitions_non_empty(self):
        for n in (5, 10, 100, 1000, 9099):
            p = make_partitions(n)
            assert p.train_size() >= 1
            assert p.cv_size() >= 1
            assert p.holdout_size() >= 1

    def test_minimum_n_rejected(self):
        with pytest.raises(ValueError):
            make_partitions(4)
        with pytest.raises(ValueError):
            make_partitions(0)

    def test_edge_case_n_5(self):
        p = make_partitions(5)
        # 60% of 5 = 3, 80% of 5 = 4, holdout = 5 - 4 = 1
        assert p.train_size() >= 1
        assert p.cv_size() >= 1
        assert p.holdout_size() >= 1
        assert p.train_end + (p.cv_end - p.cv_start) + p.holdout_size() == 5

    def test_no_observation_dropped(self):
        for n in (5, 10, 100, 1000, 9099, 12345):
            p = make_partitions(n)
            covered = p.train_size() + p.cv_size() + p.holdout_size()
            assert covered == n

    def test_slice_helpers(self):
        p = make_partitions(1000)
        assert p.train_slice() == slice(0, 600)
        assert p.cv_slice() == slice(600, 800)
        assert p.holdout_slice() == slice(800, 1000)
