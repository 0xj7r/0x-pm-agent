# Paired MM module

This module is the reusable strategy algorithm. Market descriptors define what
is traded; this module defines how paired market-making behaves.

## Decision order

1. Evaluate hard policy using filled running inventory.
2. Evaluate merge candidate.
3. Evaluate EV-gated rescue candidate.
4. Generate paired BUY-BUY entry ladder only if hard policy allows it.
5. Return typed decisions. Runtime/execution remains responsible for hard risk
   approval and venue submission.

## Ladder invariant

The entry ladder never sells. It posts maker BUY bids on both YES and NO legs.
Selling, buying the opposite leg for merge, redeeming, and merging live behind
the rescue/merge seams.

## Rescue invariant

The rescue EV brain is pure. It does not know how to walk CLOB depth or choose
IOC/FAK prices. Concrete strategies must implement rescue intent construction
behind `RescueIntentBuilder`.
