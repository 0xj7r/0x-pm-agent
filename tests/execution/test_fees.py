from shared.fees import taker_fee, taker_fee_usd


def test_taker_fee_at_half():
    assert abs(taker_fee(0.50) - 0.018) < 0.0001


def test_taker_fee_at_zero():
    assert taker_fee(0.0) == 0.0


def test_taker_fee_at_one():
    assert taker_fee(1.0) == 0.0


def test_taker_fee_symmetric():
    assert abs(taker_fee(0.3) - taker_fee(0.7)) < 0.0001


def test_taker_fee_usd():
    fee = taker_fee_usd(0.50, 100.0)
    assert abs(fee - 1.80) < 0.01
