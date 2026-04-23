"""Known wallet aliases used for stable research artifact paths."""
from __future__ import annotations

WALLET_DIR_BY_ADDRESS = {
    "0xb27bc932bf8110d8f78e55da7d5f0497a18b5b82": "unlawful-shear",
    "0xcfb103c37c0234f524c632d964ed31f117b5f694": "xuanxuan008",
    "0xe51b3d64da5b0b8a07a55f8bb3c3170237f73cad": "split-sell",
    "0x7da07b2a8b009a406198677debda46ad651b6be2": "penny-tail",
}


def wallet_dir_name(wallet: str) -> str:
    normalized = wallet.lower()
    return WALLET_DIR_BY_ADDRESS.get(normalized, normalized[-6:])
