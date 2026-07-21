---
'@velocity-exchange/sdk': patch
---

Program liquidation-safety fixes (OtterSec audit) surfaced through the bundled IDL. `liquidate_perp_pnl_for_deposit` now reverts with the new `LiquidationWorsensAccountHealth` (6361) error instead of seizing a deposit when doing so would grow the account's margin shortage (fees above the liquidation buffer). `liquidate_spot_with_swap_end` now caps its insurance-side fee by the margin shortage like the direct spot path, delivering equivalent borrow relief. `resolve_spot_bankruptcy` now enforces a deterministic perp-before-spot precedence (matching the keeper bots) and reverts with the new `PerpBankruptcyMustPrecedeSpot` (6362) error while a cross-margin perp bankruptcy is still pending, so the shared insurance-fund draw order can't be gamed to shift socialized loss.
