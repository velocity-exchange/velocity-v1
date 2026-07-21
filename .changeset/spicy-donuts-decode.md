---
'@velocity-exchange/sdk': patch
---

Fix the IDL's `SpotMarket` layout: the borsh-packed offset of `protocol_fee_pool` was 5 bytes short of the on-chain `#[repr(C)]` offset (implicit alignment padding the IDL didn't model), so `protocolFeePool`, `protocolLiquidationFee`, and `protocolFeeFactor` decoded garbage/zeros. On-chain layout is unchanged; only the IDL (and generated types) are corrected.
