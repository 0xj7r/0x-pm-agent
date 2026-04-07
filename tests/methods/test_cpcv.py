"""Tests for autoresearch.methods.cpcv."""
from __future__ import annotations

import math

import numpy as np
import pytest

from autoresearch.methods.cpcv import (
    CPCVSplit,
    aggregate_oos_distribution,
    cpcv_splits,
    make_groups,
)


class TestMakeGroups:
    def test_evenly_divisible(self):
        groups = make_groups(100, 10)
        assert len(groups) == 10
        assert all(len(g) == 10 for g in groups)
        # Concatenation should be 0..n-1
        assert np.array_equal(np.concatenate(groups), np.arange(100))

    def test_not_evenly_divisible(self):
        # 103 / 10 = 10 base, 3 extras -> first 3 groups have 11
        groups = make_groups(103, 10)
        assert [len(g) for g in groups] == [11, 11, 11, 10, 10, 10, 10, 10, 10, 10]
        assert np.array_equal(np.concatenate(groups), np.arange(103))

    def test_invalid_partitions(self):
        with pytest.raises(ValueError):
            make_groups(100, 1)
        with pytest.raises(ValueError):
            make_groups(5, 10)


class TestCPCVSplits:
    def test_default_split_count(self):
        # C(10, 2) = 45
        splits = cpcv_splits(n=100, n_partitions=10, k_test=2)
        assert len(splits) == 45

    def test_train_test_disjoint(self):
        splits = cpcv_splits(n=100, n_partitions=10, k_test=2)
        for s in splits:
            train_set = set(int(i) for i in s.train_indices)
            test_set = set(int(i) for i in s.test_indices)
            assert train_set.isdisjoint(test_set)

    def test_test_size_matches_k_test_groups(self):
        n = 100
        splits = cpcv_splits(n=n, n_partitions=10, k_test=2)
        # Each test should be 2 groups of 10 = 20 obs (no embargo)
        for s in splits:
            assert len(s.test_indices) == 20

    def test_embargo_removes_training_observations(self):
        n = 100
        no_embargo = cpcv_splits(n=n, n_partitions=10, k_test=2, embargo=0)
        with_embargo = cpcv_splits(n=n, n_partitions=10, k_test=2, embargo=3)
        # Train sets should be smaller with embargo
        for s_no, s_yes in zip(no_embargo, with_embargo):
            assert len(s_yes.train_indices) <= len(s_no.train_indices)
        # At least one split should be strictly smaller (some test
        # group has neighbors in the training set)
        any_strictly_smaller = any(
            len(s_yes.train_indices) < len(s_no.train_indices)
            for s_no, s_yes in zip(no_embargo, with_embargo)
        )
        assert any_strictly_smaller

    def test_embargo_indices_excluded_from_train(self):
        n = 100
        splits = cpcv_splits(n=n, n_partitions=10, k_test=2, embargo=3)
        for s in splits:
            train_set = set(int(i) for i in s.train_indices)
            test_set = set(int(i) for i in s.test_indices)
            # For each test group, the 3 indices on each side should
            # not appear in train_set
            for g in s.test_groups:
                # Manually compute group bounds
                base = n // 10
                start = g * base
                end = start + base
                for j in range(max(0, start - 3), start):
                    if j not in test_set:
                        assert j not in train_set
                for j in range(end, min(n, end + 3)):
                    if j not in test_set:
                        assert j not in train_set

    def test_invalid_k_test(self):
        with pytest.raises(ValueError):
            cpcv_splits(n=100, n_partitions=10, k_test=0)
        with pytest.raises(ValueError):
            cpcv_splits(n=100, n_partitions=10, k_test=10)

    def test_negative_embargo(self):
        with pytest.raises(ValueError):
            cpcv_splits(n=100, n_partitions=10, k_test=2, embargo=-1)


class TestAggregateOOS:
    def test_basic_stats(self):
        results = [1.0, 2.0, 3.0, 4.0, 5.0]
        agg = aggregate_oos_distribution(results)
        assert agg["n"] == 5
        assert agg["mean"] == pytest.approx(3.0)
        assert agg["min"] == 1.0
        assert agg["max"] == 5.0
        assert agg["median"] == 3.0

    def test_empty_input(self):
        assert aggregate_oos_distribution([]) == {"n": 0}
