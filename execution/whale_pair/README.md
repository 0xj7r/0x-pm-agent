# Whale Pair Strategy Toolkit

This folder groups whale-pair strategy modules and reusable configuration.

## Canonical command locations

- `scripts/whale_pair/cmd/whale_pair_compare_variants.py`
- `scripts/whale_pair/cmd/whale_pair_w1_calibrate.py`
- `scripts/whale_pair/cmd/whale_pair_w1_validation.py`
- `scripts/whale_pair/cmd/whale_pair_walkforward.py`
- `scripts/whale_pair/cmd/whale_pair_w1_report.py`
- `scripts/whale_pair/cmd/whale_pair_rust_paper_variants.sh`
- `scripts/whale_pair/cmd/whale_pair_rust_paper_remote.sh`

Top-level wrappers remain for backwards compatibility:

- `scripts/whale_pair_compare_variants.py`
- `scripts/whale_pair_w1_calibrate.py`
- `scripts/whale_pair_w1_validation.py`
- `scripts/whale_pair_walkforward.py`
- `scripts/whale_pair_w1_report.py`
- `scripts/whale_pair_rust_paper_variants.sh`
- `scripts/whale_pair_rust_paper_remote.sh`

Legacy one-shot shims now defer to the canonical command under
`scripts/whale_pair/cmd/` through a shared dispatch convention shared by root-level
wrappers.

For programmatic imports, import command modules from `scripts.whale_pair.cmd`:

```python
from scripts.whale_pair import cmd
```

## Shared settings

- Strategy presets: `scripts/whale_pair/strategy_presets.py`
- Shared path helper: `scripts/whale_pair/_paths.py`
