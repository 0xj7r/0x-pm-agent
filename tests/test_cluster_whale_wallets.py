from __future__ import annotations

from scripts.cluster_whale_wallets import WalletProfile, pair_overlap


def _profile(wallet: str, pseudonym: str, rows: list[dict]):
    return WalletProfile(
        wallet=wallet,
        suffix=wallet[-6:],
        pseudonym=pseudonym,
        activity_path=None,  # type: ignore[arg-type]
        rows=rows,
    )


def test_pair_overlap_counts_shared_markets_and_synced_rows():
    left = _profile(
        "0xaaa111",
        "left",
        [
            {"type": "TRADE", "slug": "btc-updown-5m-1", "timestamp": 100, "conditionId": "c1"},
            {"type": "TRADE", "slug": "btc-updown-5m-2", "timestamp": 200, "conditionId": "c2"},
        ],
    )
    right = _profile(
        "0xbbb222",
        "right",
        [
            {"type": "TRADE", "slug": "btc-updown-5m-1", "timestamp": 101, "conditionId": "c1"},
            {"type": "TRADE", "slug": "btc-updown-5m-3", "timestamp": 240, "conditionId": "c3"},
        ],
    )

    overlap = pair_overlap(left, right)
    assert overlap["shared_slugs"] == 1
    assert overlap["shared_condition_ids"] == 1
    assert overlap["synced_trade_rows_2s"] == 1
    assert overlap["synced_trade_rows_10s"] == 1
    assert overlap["slug_jaccard"] > 0
