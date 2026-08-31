/**
 * Program IDL in camelCase format in order to be used in JS/TS.
 *
 * Note that this is only a type helper and is not the actual IDL. The original
 * IDL can be found at `target/idl/jit_proxy.json`.
 */
export type JitProxy = {
	address: 'J1TPRoXCtGuMcWiWFE6RB9eZU8U35PBMETCwNQLCNPhQ';
	metadata: {
		name: 'jitProxy';
		version: '0.21.0';
		spec: '0.1.0';
		description: 'Created with Anchor';
	};
	instructions: [
		{
			name: 'arbPerp';
			discriminator: [116, 105, 138, 99, 28, 171, 39, 225];
			accounts: [
				{
					name: 'state';
				},
				{
					name: 'user';
					writable: true;
				},
				{
					name: 'userStats';
					writable: true;
				},
				{
					name: 'authority';
					signer: true;
				},
				{
					name: 'velocityProgram';
					address: 'vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P';
				},
			];
			args: [
				{
					name: 'marketIndex';
					type: 'u16';
				},
			];
		},
		{
			name: 'checkOrderConstraints';
			discriminator: [183, 174, 142, 245, 5, 29, 207, 2];
			accounts: [
				{
					name: 'state';
					docs: [
						"Velocity's `State`, read for the live slot duration so the oracle",
						'staleness windows here match the ones velocity itself applies. The PDA',
						'belongs to the velocity program, so the derivation names it explicitly.',
					];
					pda: {
						seeds: [
							{
								kind: 'const';
								value: [
									118,
									101,
									108,
									111,
									99,
									105,
									116,
									121,
									95,
									115,
									116,
									97,
									116,
									101,
								];
							},
						];
						program: {
							kind: 'const';
							value: [
								13,
								162,
								222,
								50,
								93,
								130,
								241,
								222,
								120,
								205,
								77,
								177,
								103,
								33,
								15,
								103,
								45,
								147,
								250,
								167,
								129,
								184,
								165,
								217,
								84,
								183,
								159,
								1,
								88,
								249,
								227,
								150,
							];
						};
					};
				},
				{
					name: 'user';
				},
			];
			args: [
				{
					name: 'constraints';
					type: {
						vec: {
							defined: {
								name: 'orderConstraint';
							};
						};
					};
				},
			];
		},
		{
			name: 'jit';
			discriminator: [99, 42, 97, 140, 152, 62, 167, 234];
			accounts: [
				{
					name: 'state';
				},
				{
					name: 'user';
					writable: true;
				},
				{
					name: 'userStats';
					writable: true;
				},
				{
					name: 'taker';
					writable: true;
				},
				{
					name: 'takerStats';
					writable: true;
				},
				{
					name: 'authority';
					signer: true;
				},
				{
					name: 'velocityProgram';
					address: 'vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P';
				},
			];
			args: [
				{
					name: 'params';
					type: {
						defined: {
							name: 'jitParams';
						};
					};
				},
			];
		},
		{
			name: 'jitSignedMsg';
			discriminator: [134, 130, 156, 72, 37, 120, 153, 21];
			accounts: [
				{
					name: 'state';
				},
				{
					name: 'user';
					writable: true;
				},
				{
					name: 'userStats';
					writable: true;
				},
				{
					name: 'taker';
					writable: true;
				},
				{
					name: 'takerStats';
					writable: true;
				},
				{
					name: 'takerSignedMsgUserOrders';
					writable: true;
				},
				{
					name: 'authority';
					signer: true;
				},
				{
					name: 'velocityProgram';
					address: 'vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P';
				},
			];
			args: [
				{
					name: 'params';
					type: {
						defined: {
							name: 'jitSignedMsgParams';
						};
					};
				},
			];
		},
	];
	errors: [
		{
			code: 6000;
			name: 'bidNotCrossed';
			msg: 'bidNotCrossed';
		},
		{
			code: 6001;
			name: 'askNotCrossed';
			msg: 'askNotCrossed';
		},
		{
			code: 6002;
			name: 'takerOrderNotFound';
			msg: 'takerOrderNotFound';
		},
		{
			code: 6003;
			name: 'orderSizeBreached';
			msg: 'orderSizeBreached';
		},
		{
			code: 6004;
			name: 'noBestBid';
			msg: 'noBestBid';
		},
		{
			code: 6005;
			name: 'noBestAsk';
			msg: 'noBestAsk';
		},
		{
			code: 6006;
			name: 'noArbOpportunity';
			msg: 'noArbOpportunity';
		},
		{
			code: 6007;
			name: 'unprofitableArb';
			msg: 'unprofitableArb';
		},
		{
			code: 6008;
			name: 'positionLimitBreached';
			msg: 'positionLimitBreached';
		},
		{
			code: 6009;
			name: 'noFill';
			msg: 'noFill';
		},
		{
			code: 6010;
			name: 'signedMsgOrderDoesNotExist';
			msg: 'signedMsgOrderDoesNotExist';
		},
		{
			code: 6011;
			name: 'spotOrdersNotSupported';
			msg: 'spotOrdersNotSupported';
		},
	];
	types: [
		{
			name: 'feeStructure';
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'feeTiers';
						type: {
							array: [
								{
									defined: {
										name: 'feeTier';
									};
								},
								10,
							];
						};
					},
					{
						name: 'fillerRewardStructure';
						type: {
							defined: {
								name: 'orderFillerRewardStructure';
							};
						};
					},
					{
						name: 'flatFillerFee';
						type: 'u64';
					},
					{
						name: 'ammFeeNumerator';
						docs: [
							'Share of the trade-fee *remainder* (taker fee after maker rebate, referral,',
							'referee discount, and filler reward are taken off the top) provisioned to',
							'the AMM as liquidity (its backstop-of-last-resort tranche, tracked in',
							'`PerpMarket.fee_ledger.amm_protocol_fees_received` alongside the vAMM',
							'maker rebate when that feature is enabled). precision:',
							'FEE_PERCENTAGE_DENOMINATOR. `amm_fee_numerator + if_fee_numerator` must',
							'be <= FEE_PERCENTAGE_DENOMINATOR; the protocol receives the residual',
							'(`remainder − amm − if`) into its withdrawable `protocol_fee_pool`.',
							'(Was the reserved `padding: u64`, repartitioned into two u32s —',
							'size/alignment unchanged.)',
						];
						type: 'u32';
					},
					{
						name: 'ifFeeNumerator';
						docs: [
							'Share of the trade-fee remainder routed to the insurance fund (`revenue_pool`).',
						];
						type: 'u32';
					},
				];
			};
		},
		{
			name: 'feeTier';
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'feeNumerator';
						type: 'u32';
					},
					{
						name: 'feeDenominator';
						type: 'u32';
					},
					{
						name: 'makerRebateNumerator';
						type: 'u32';
					},
					{
						name: 'makerRebateDenominator';
						type: 'u32';
					},
					{
						name: 'referrerRewardNumerator';
						type: 'u32';
					},
					{
						name: 'referrerRewardDenominator';
						type: 'u32';
					},
					{
						name: 'refereeFeeNumerator';
						type: 'u32';
					},
					{
						name: 'refereeFeeDenominator';
						type: 'u32';
					},
				];
			};
		},
		{
			name: 'jitParams';
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'takerOrderId';
						type: 'u32';
					},
					{
						name: 'maxPosition';
						type: 'i64';
					},
					{
						name: 'minPosition';
						type: 'i64';
					},
					{
						name: 'bid';
						type: 'i64';
					},
					{
						name: 'ask';
						type: 'i64';
					},
					{
						name: 'priceType';
						type: {
							defined: {
								name: 'priceType';
							};
						};
					},
					{
						name: 'postOnly';
						type: {
							option: {
								defined: {
									name: 'postOnlyParam';
								};
							};
						};
					},
				];
			};
		},
		{
			name: 'jitSignedMsgParams';
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'signedMsgOrderUuid';
						type: {
							array: ['u8', 8];
						};
					},
					{
						name: 'maxPosition';
						type: 'i64';
					},
					{
						name: 'minPosition';
						type: 'i64';
					},
					{
						name: 'bid';
						type: 'i64';
					},
					{
						name: 'ask';
						type: 'i64';
					},
					{
						name: 'priceType';
						type: {
							defined: {
								name: 'priceType';
							};
						};
					},
					{
						name: 'postOnly';
						type: {
							option: {
								defined: {
									name: 'postOnlyParam';
								};
							};
						};
					},
				];
			};
		},
		{
			name: 'oracleGuardRails';
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'priceDivergence';
						type: {
							defined: {
								name: 'priceDivergenceGuardRails';
							};
						};
					},
					{
						name: 'validity';
						type: {
							defined: {
								name: 'validityGuardRails';
							};
						};
					},
				];
			};
		},
		{
			name: 'order';
			serialization: 'bytemuckunsafe';
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'slot';
						docs: ['The slot the order was placed'];
						type: 'u64';
					},
					{
						name: 'price';
						docs: [
							'The limit price for the order (can be 0 for market orders)',
							"For orders with an auction, this price isn't used until the auction is complete",
							'precision: PRICE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'baseAssetAmount';
						docs: [
							'The size of the order',
							'precision for perps: BASE_PRECISION',
							'precision for spot: token mint precision',
						];
						type: 'u64';
					},
					{
						name: 'baseAssetAmountFilled';
						docs: [
							'The amount of the order filled',
							'precision for perps: BASE_PRECISION',
							'precision for spot: token mint precision',
						];
						type: 'u64';
					},
					{
						name: 'quoteAssetAmountFilled';
						docs: [
							'The amount of quote filled for the order',
							'precision: QUOTE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'triggerPrice';
						docs: [
							'At what price the order will be triggered. Only relevant for trigger orders',
							'precision: PRICE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'auctionStartPrice';
						docs: [
							'The start price for the auction. Only relevant for market/oracle orders',
							'precision: PRICE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'auctionEndPrice';
						docs: [
							'The end price for the auction. Only relevant for market/oracle orders',
							'precision: PRICE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'maxTs';
						docs: ['The time when the order will expire'];
						type: 'i64';
					},
					{
						name: 'oraclePriceOffset';
						docs: [
							'If set, the order limit price is the oracle price + this offset',
							'precision: PRICE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'orderId';
						docs: [
							'The id for the order. Each users has their own order id space',
						];
						type: 'u32';
					},
					{
						name: 'marketIndex';
						docs: ['The perp/spot market index'];
						type: 'u16';
					},
					{
						name: 'status';
						docs: ['Whether the order is open or unused'];
						type: {
							defined: {
								name: 'orderStatus';
							};
						};
					},
					{
						name: 'orderType';
						docs: ['The type of order'];
						type: {
							defined: {
								name: 'orderType';
							};
						};
					},
					{
						name: 'marketType';
						docs: ['Whether market is spot or perp'];
						type: {
							defined: {
								name: 'velocity::state::user::MarketType';
							};
						};
					},
					{
						name: 'userOrderId';
						docs: [
							'User generated order id. Can make it easier to place/cancel orders',
						];
						type: 'u8';
					},
					{
						name: 'existingPositionDirection';
						docs: ['What the users position was when the order was placed'];
						type: {
							defined: {
								name: 'positionDirection';
							};
						};
					},
					{
						name: 'direction';
						docs: [
							'Whether the user is going long or short. LONG = bid, SHORT = ask',
						];
						type: {
							defined: {
								name: 'positionDirection';
							};
						};
					},
					{
						name: 'reduceOnly';
						docs: ['Whether the order is allowed to only reduce position size'];
						type: 'bool';
					},
					{
						name: 'postOnly';
						docs: ['Whether the order must be a maker'];
						type: 'bool';
					},
					{
						name: 'immediateOrCancel';
						docs: [
							'Whether the order must be canceled the same slot it is placed',
						];
						type: 'bool';
					},
					{
						name: 'triggerCondition';
						docs: [
							'Whether the order is triggered above or below the trigger price. Only relevant for trigger orders',
						];
						type: {
							defined: {
								name: 'orderTriggerCondition';
							};
						};
					},
					{
						name: 'auctionDuration';
						docs: [
							'Auction length in wall clock 400ms units (one slot at the 400ms',
							'baseline, where the raw value is identical to the historical slot',
							"count). Progress compares `SlotClock::elapsed` against this value's",
							'wall clock length, so the ramp holds at every slot duration and the',
							'u8 keeps the full historical 72s range.',
						];
						type: 'u8';
					},
					{
						name: 'postedSlotTail';
						docs: [
							'Last 8 bits of the slot the order was posted onchain (not order slot for signed msg orders)',
						];
						type: 'u8';
					},
					{
						name: 'bitFlags';
						docs: [
							'Bitflags for further classification',
							'0: is_signed_message',
						];
						type: 'u8';
					},
					{
						name: 'padding';
						type: {
							array: ['u8', 5];
						};
					},
				];
			};
		},
		{
			name: 'orderConstraint';
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'maxPosition';
						type: 'i64';
					},
					{
						name: 'minPosition';
						type: 'i64';
					},
					{
						name: 'marketIndex';
						type: 'u16';
					},
					{
						name: 'marketType';
						type: {
							defined: {
								name: 'jit_proxy::state::MarketType';
							};
						};
					},
				];
			};
		},
		{
			name: 'orderFillerRewardStructure';
			docs: [
				'`u128` is placed first so `#[repr(C)]` layout matches between host (x86_64,',
				'align 16 in Rust ≥ 1.77) and the SBF VM (align 8). Trailing `_padding`',
				'rounds the struct to a host-portable 32 bytes. See',
				'`docs/alignment-and-native-offsets.md`.',
			];
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'timeBasedRewardLowerBound';
						type: 'u128';
					},
					{
						name: 'rewardNumerator';
						type: 'u32';
					},
					{
						name: 'rewardDenominator';
						type: 'u32';
					},
					{
						name: 'padding';
						type: {
							array: ['u8', 8];
						};
					},
				];
			};
		},
		{
			name: 'orderStatus';
			type: {
				kind: 'enum';
				variants: [
					{
						name: 'init';
					},
					{
						name: 'open';
					},
					{
						name: 'filled';
					},
					{
						name: 'canceled';
					},
				];
			};
		},
		{
			name: 'orderTriggerCondition';
			type: {
				kind: 'enum';
				variants: [
					{
						name: 'above';
					},
					{
						name: 'below';
					},
					{
						name: 'triggeredAbove';
					},
					{
						name: 'triggeredBelow';
					},
				];
			};
		},
		{
			name: 'orderType';
			type: {
				kind: 'enum';
				variants: [
					{
						name: 'market';
					},
					{
						name: 'limit';
					},
					{
						name: 'triggerMarket';
					},
					{
						name: 'triggerLimit';
					},
					{
						name: 'oracle';
					},
				];
			};
		},
		{
			name: 'perpPosition';
			serialization: 'bytemuckunsafe';
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'lastCumulativeFundingRate';
						docs: [
							"The perp market's last cumulative funding rate. Used to calculate the funding payment owed to user",
							'precision: FUNDING_RATE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'baseAssetAmount';
						docs: [
							'the size of the users perp position',
							'precision: BASE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'quoteAssetAmount';
						docs: [
							'Used to calculate the users pnl. Upon entry, is equal to base_asset_amount * avg entry price - fees',
							'Updated when the user open/closes position or settles pnl. Includes fees/funding',
							'precision: QUOTE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'quoteBreakEvenAmount';
						docs: [
							'The amount of quote the user would need to exit their position at to break even',
							'Updated when the user open/closes position or settles pnl. Includes fees/funding',
							'precision: QUOTE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'quoteEntryAmount';
						docs: [
							'The amount quote the user entered the position with. Equal to base asset amount * avg entry price',
							'Updated when the user open/closes position. Excludes fees/funding',
							'precision: QUOTE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'openBids';
						docs: [
							'The amount of non reduce only trigger orders the user has open',
							'precision: BASE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'openAsks';
						docs: [
							'The amount of non reduce only trigger orders the user has open',
							'precision: BASE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'settledPnl';
						docs: [
							'The amount of pnl settled in this market since opening the position',
							'precision: QUOTE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'isolatedPositionScaledBalance';
						docs: [
							'The scaled balance of the isolated position',
							'precision: SPOT_BALANCE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'padding';
						type: {
							array: ['u8', 2];
						};
					},
					{
						name: 'maxMarginRatio';
						type: 'u16';
					},
					{
						name: 'marketIndex';
						docs: ['The market index for the perp market'];
						type: 'u16';
					},
					{
						name: 'openOrders';
						docs: ['The number of open orders'];
						type: 'u8';
					},
					{
						name: 'positionFlag';
						type: 'u8';
					},
				];
			};
		},
		{
			name: 'positionDirection';
			type: {
				kind: 'enum';
				variants: [
					{
						name: 'long';
					},
					{
						name: 'short';
					},
				];
			};
		},
		{
			name: 'postOnlyParam';
			type: {
				kind: 'enum';
				variants: [
					{
						name: 'none';
					},
					{
						name: 'mustPostOnly';
					},
					{
						name: 'tryPostOnly';
					},
					{
						name: 'slide';
					},
				];
			};
		},
		{
			name: 'priceDivergenceGuardRails';
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'markOraclePercentDivergence';
						type: 'u64';
					},
					{
						name: 'oracleTwap5minPercentDivergence';
						type: 'u64';
					},
				];
			};
		},
		{
			name: 'priceType';
			type: {
				kind: 'enum';
				variants: [
					{
						name: 'limit';
					},
					{
						name: 'oracle';
					},
				];
			};
		},
		{
			name: 'spotBalanceType';
			type: {
				kind: 'enum';
				variants: [
					{
						name: 'deposit';
					},
					{
						name: 'borrow';
					},
				];
			};
		},
		{
			name: 'spotPosition';
			serialization: 'bytemuckunsafe';
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'scaledBalance';
						docs: [
							'The scaled balance of the position. To get the token amount, multiply by the cumulative deposit/borrow',
							'interest of corresponding market.',
							'precision: SPOT_BALANCE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'openBids';
						docs: [
							'How many spot non reduce only trigger orders the user has open',
							'precision: token mint precision',
						];
						type: 'i64';
					},
					{
						name: 'openAsks';
						docs: [
							'How many spot non reduce only trigger orders the user has open',
							'precision: token mint precision',
						];
						type: 'i64';
					},
					{
						name: 'cumulativeDeposits';
						docs: [
							'The cumulative deposits/borrows a user has made into a market',
							'precision: token mint precision',
						];
						type: 'i64';
					},
					{
						name: 'marketIndex';
						docs: ['The market index of the corresponding spot market'];
						type: 'u16';
					},
					{
						name: 'balanceType';
						docs: ['Whether the position is deposit or borrow'];
						type: {
							defined: {
								name: 'spotBalanceType';
							};
						};
					},
					{
						name: 'openOrders';
						docs: ['Number of open orders'];
						type: 'u8';
					},
					{
						name: 'padding';
						type: {
							array: ['u8', 4];
						};
					},
				];
			};
		},
		{
			name: 'state';
			serialization: 'bytemuckunsafe';
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'coldAdmin';
						docs: [
							'Root authority. Set at `initialize`; only this key can rotate `warm_admin`',
							'and `pause_admin`. Expected to sit behind a (small) timelocked multisig.',
						];
						type: 'pubkey';
					},
					{
						name: 'warmAdmin';
						docs: [
							'Operational authority (e.g. multisig+timelock). Can rotate the 10 hot keys',
							'below. `Pubkey::default()` means unset — only `cold_admin` can act in that case.',
						];
						type: 'pubkey';
					},
					{
						name: 'pauseAdmin';
						docs: [
							'Emergency pause authority. No onchain timelock — intended to live behind a',
							'fast-acting multisig that can flip pause flags without delay. May only *add*',
							'pause bits (never clear them); cold/warm retain full pause + unpause power.',
							'`Pubkey::default()` means unassigned (only cold/warm can pause).',
						];
						type: 'pubkey';
					},
					{
						name: 'hotAmmCrank';
						docs: [
							'Purpose-specific bot keys. `Pubkey::default()` means the role is unassigned',
							'and only warm/cold can call handlers gated on that role.',
						];
						type: 'pubkey';
					},
					{
						name: 'hotLpCache';
						type: 'pubkey';
					},
					{
						name: 'hotLpSwap';
						type: 'pubkey';
					},
					{
						name: 'hotLpSettle';
						type: 'pubkey';
					},
					{
						name: 'hotFeatureFlag';
						type: 'pubkey';
					},
					{
						name: 'hotFuel';
						type: 'pubkey';
					},
					{
						name: 'hotUserFlag';
						type: 'pubkey';
					},
					{
						name: 'hotVaultDeposit';
						type: 'pubkey';
					},
					{
						name: 'hotMmOracleCrank';
						type: 'pubkey';
					},
					{
						name: 'hotAmmSpreadAdjust';
						docs: [
							'Bot authority for the low-CU native AMM spread-adjustment crank.',
						];
						type: 'pubkey';
					},
					{
						name: 'whitelistMint';
						type: 'pubkey';
					},
					{
						name: 'discountMint';
						type: 'pubkey';
					},
					{
						name: 'signer';
						type: 'pubkey';
					},
					{
						name: 'srmVault';
						type: 'pubkey';
					},
					{
						name: 'perpFeeStructure';
						type: {
							defined: {
								name: 'feeStructure';
							};
						};
					},
					{
						name: 'spotFeeStructure';
						type: {
							defined: {
								name: 'feeStructure';
							};
						};
					},
					{
						name: 'oracleGuardRails';
						type: {
							defined: {
								name: 'oracleGuardRails';
							};
						};
					},
					{
						name: 'numberOfAuthorities';
						type: 'u64';
					},
					{
						name: 'numberOfSubAccounts';
						type: 'u64';
					},
					{
						name: 'liquidationMarginBufferRatio';
						type: 'u32';
					},
					{
						name: 'settlementDuration';
						type: 'u16';
					},
					{
						name: 'numberOfMarkets';
						type: 'u16';
					},
					{
						name: 'numberOfSpotMarkets';
						type: 'u16';
					},
					{
						name: 'signerNonce';
						type: 'u8';
					},
					{
						name: 'minPerpAuctionDuration';
						docs: [
							'Compact wall-clock duration encoded in historical 400ms slot quanta.',
						];
						type: 'u8';
					},
					{
						name: 'defaultMarketOrderTimeInForce';
						docs: [
							'Default time-in-force for market orders, in seconds. `Order.max_ts` is a',
							'unix timestamp, so this never converts through the slot length and stays',
							'a raw integer. It currently has no onchain reader.',
						];
						type: 'u8';
					},
					{
						name: 'defaultSpotAuctionDuration';
						docs: [
							'An actual slot-count setting, not a wall-clock duration. It currently has',
							'no onchain reader (spot DLOB trading is disabled), so it intentionally',
							'remains raw rather than using `StoredSlotDuration`.',
						];
						type: 'u8';
					},
					{
						name: 'exchangeStatus';
						type: 'u8';
					},
					{
						name: 'liquidationDuration';
						docs: [
							'Compact wall-clock duration encoded in historical 400ms slot quanta.',
						];
						type: 'u8';
					},
					{
						name: 'initialPctToLiquidate';
						type: 'u16';
					},
					{
						name: 'maxNumberOfSubAccounts';
						type: 'u16';
					},
					{
						name: 'maxInitializeUserFee';
						type: 'u16';
					},
					{
						name: 'featureBitFlags';
						type: 'u8';
					},
					{
						name: 'lpPoolFeatureBitFlags';
						type: 'u8';
					},
					{
						name: 'solvencyStatus';
						docs: [
							'Bitmask of `SolvencyStatus` flags. Gates internal solvency-repair flows',
							'(bankruptcy / pnl-deficit resolution) independently of `WithdrawPaused`,',
							'so user withdrawals can be halted while repair keeps running, or repair',
							'can be frozen on its own when an oracle is suspect. `0` = repair allowed.',
						];
						type: 'u8';
					},
					{
						name: 'protocolFeeRecipientPerp';
						docs: [
							'Treasury that PERP protocol fees (quote-denominated) may be withdrawn',
							'to. Settable only by `cold_admin`. `withdraw_protocol_fees_perp` pays',
							"this key's associated token account (recipient-locked).",
							'`Pubkey::default()` (unset) makes perp withdrawals inert.',
						];
						type: 'pubkey';
					},
					{
						name: 'protocolFeeRecipientSpot';
						docs: [
							"Treasury that SPOT protocol fees (each market's own token: lending",
							'carveouts + spot-liquidation cuts) may be withdrawn to. Settable only',
							"by `cold_admin`. `withdraw_protocol_fees_spot` pays this key's",
							"associated token account for the market's mint (recipient-locked).",
							'`Pubkey::default()` (unset) makes spot withdrawals inert.',
						];
						type: 'pubkey';
					},
					{
						name: 'hotFeeWithdraw';
						docs: [
							'Hot key authorized for the `FeeWithdraw` role (triggers protocol-fee',
							'withdrawals to the configured recipients).',
						];
						type: 'pubkey';
					},
					{
						name: 'hotAccountExtension';
						docs: [
							'Hot key authorized for the `AccountExtension` role (grows zero-copy',
							"accounts to the deployed program's size after a struct-extending",
							'upgrade).',
						];
						type: 'pubkey';
					},
					{
						name: 'promoFeeTier';
						docs: [
							'Promotional fee-tier floor applied to every account: the effective',
							'perp fee tier is `max(volume tier, promo_fee_tier)` (clamped to the',
							'configured tier count), so nobody is downgraded by it. 0 = no-op',
							'(disabled), also what pre-upgrade accounts read from former padding.',
							'Reset to 0 and every account is back on its volume tier at its next',
							'fill; no per-user state.',
						];
						type: 'u8';
					},
					{
						name: 'slotDurationMs';
						docs: [
							'Legacy current slot duration field in milliseconds, kept coherent by',
							'the permissionless sync as the IBRL feature gates activate',
							'(400 -> 350 -> 300 -> 250 -> 200). `0` means unset (what pre upgrade',
							'accounts read out of former padding) and is interpreted as the 400ms',
							'baseline. Never read this field directly, use [`State::slot_clock`] /',
							'[`State::slot_duration`]; once any `slot_duration_transition_slots`',
							'entry is set the archive is authoritative over this field.',
						];
						type: 'u16';
					},
					{
						name: 'pendingSlotDurationMs';
						docs: [
							'Legacy staged next slot duration in ms, kept coherent by the',
							'permissionless sync for older readers. `0` means nothing is staged. Once',
							'`slot_duration_effective_slot` is reached, the legacy resolution returns',
							'this value instead of `slot_duration_ms`. Superseded by the transition',
							'archive.',
						];
						type: 'u16';
					},
					{
						name: 'slotDurationPad';
						docs: [
							'Explicit padding so `slot_duration_effective_slot` (u64) lands on its',
							'8-byte alignment with no *implicit* padding (see the alignment invariant).',
						];
						type: {
							array: ['u8', 2];
						};
					},
					{
						name: 'slotDurationEffectiveSlot';
						docs: [
							'Slot at which `pending_slot_duration_ms` takes effect: the first slot of',
							"the epoch after the target gate's activation epoch, derived from the",
							'`EpochSchedule` sysvar at sync time. `0` when nothing is staged.',
						];
						type: 'u64';
					},
					{
						name: 'slotDurationTransitionSlots';
						docs: [
							'First slot of each post baseline IBRL regime, ordered as',
							'`[350ms, 300ms, 250ms, 200ms]`. Zero means that transition has not been',
							'synchronized yet. These anchors let elapsed time math integrate an',
							'interval piecewise instead of multiplying its whole slot delta by the',
							'duration at one endpoint.',
						];
						type: {
							array: ['u64', 4];
						};
					},
					{
						name: 'hotVammQuoteManagement';
						docs: [
							'Active-management authority for scoped vAMM quoting controls carried by',
							'`HotAdminUpdatePerpMarket`. This may be a multisig PDA; timelock policy',
							'lives in that multisig. Added from former padding so existing fields,',
							'including `hot_amm_spread_adjust`, retain their offsets.',
						];
						type: 'pubkey';
					},
					{
						name: 'padding';
						docs: [
							'168 = the former 244 byte padding minus the 12 staging bytes, the 32 bytes',
							'used by `slot_duration_transition_slots`, and the 32-byte quote-management',
							'authority.',
							'(`pending_slot_duration_ms` 2 + `slot_duration_pad` 2 + the 8-byte',
							'`slot_duration_effective_slot`). The padding still absorbs the 8 bytes that',
							'were previously *implicit* trailing padding on x86_64 (State contains a',
							'u128, align 16 on the host but 8 on SBF; explicit padding keeps',
							'`size_of::<State>()` 1744 on both targets, per the alignment invariant in',
							'docs/alignment-and-native-offsets.md).',
						];
						type: {
							array: ['u8', 168];
						};
					},
				];
			};
		},
		{
			name: 'user';
			serialization: 'bytemuckunsafe';
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'authority';
						docs: ['The owner/authority of the account'];
						type: 'pubkey';
					},
					{
						name: 'delegate';
						docs: [
							"An addresses that can control the account on the authority's behalf. Has limited power, cant withdraw",
						];
						type: 'pubkey';
					},
					{
						name: 'name';
						docs: ['Encoded display name e.g. "toly"'];
						type: {
							array: ['u8', 32];
						};
					},
					{
						name: 'spotPositions';
						docs: ["The user's spot positions"];
						type: {
							array: [
								{
									defined: {
										name: 'spotPosition';
									};
								},
								8,
							];
						};
					},
					{
						name: 'perpPositions';
						docs: ["The user's perp positions"];
						type: {
							array: [
								{
									defined: {
										name: 'perpPosition';
									};
								},
								8,
							];
						};
					},
					{
						name: 'orders';
						docs: ["The user's orders"];
						type: {
							array: [
								{
									defined: {
										name: 'order';
									};
								},
								32,
							];
						};
					},
					{
						name: 'totalDeposits';
						docs: [
							'The total values of deposits the user has made',
							'precision: QUOTE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'totalWithdraws';
						docs: [
							'The total values of withdrawals the user has made',
							'precision: QUOTE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'totalSocialLoss';
						docs: [
							'The total socialized loss the users has incurred upon the protocol',
							'precision: QUOTE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'settledPerpPnl';
						docs: [
							'Fees (taker fees, maker rebate, referrer reward, filler reward) and pnl for perps',
							'precision: QUOTE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'cumulativeSpotFees';
						docs: [
							'Fees (taker fees, maker rebate, filler reward) for spot',
							'precision: QUOTE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'cumulativePerpFunding';
						docs: [
							'Cumulative funding paid/received for perps',
							'precision: QUOTE_PRECISION',
						];
						type: 'i64';
					},
					{
						name: 'liquidationMarginFreed';
						docs: [
							'The amount of margin freed during liquidation. Used to force the liquidation to occur over a period of time',
							'Defaults to zero when not being liquidated',
							'precision: QUOTE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'lastActiveSlot';
						docs: [
							'The last slot a user was active. Used to determine if a user is idle',
						];
						type: 'u64';
					},
					{
						name: 'nextOrderId';
						docs: [
							'Every user order has an order id. This is the next order id to be used',
						];
						type: 'u32';
					},
					{
						name: 'maxMarginRatio';
						docs: ['Custom max initial margin ratio for the user'];
						type: 'u32';
					},
					{
						name: 'nextLiquidationId';
						docs: ['The next liquidation id to be used for user'];
						type: 'u16';
					},
					{
						name: 'subAccountId';
						docs: ['The sub account id for this user'];
						type: 'u16';
					},
					{
						name: 'status';
						docs: ['Whether the user is active, being liquidated or bankrupt'];
						type: 'u8';
					},
					{
						name: 'isMarginTradingEnabled';
						docs: ['Whether the user has enabled margin trading'];
						type: 'bool';
					},
					{
						name: 'idle';
						docs: [
							"User is idle if they haven't interacted with the protocol in 1 week and they have no orders, perp positions or borrows",
							'Off-chain keeper bots can ignore users that are idle',
						];
						type: 'bool';
					},
					{
						name: 'openOrders';
						docs: ['number of open orders'];
						type: 'u8';
					},
					{
						name: 'hasOpenOrder';
						docs: ['Whether or not user has open order'];
						type: 'bool';
					},
					{
						name: 'openAuctions';
						docs: ['number of open orders with auction'];
						type: 'u8';
					},
					{
						name: 'hasOpenAuction';
						docs: ['Whether or not user has open order with auction'];
						type: 'bool';
					},
					{
						name: 'poolId';
						type: 'u8';
					},
					{
						name: 'specialUserStatus';
						docs: ['Whether the user is a special user (vamm hedger, etc)'];
						type: 'u8';
					},
					{
						name: 'padding';
						type: {
							array: ['u8', 3];
						};
					},
					{
						name: 'equityFloor';
						docs: [
							'Minimum account net equity (unweighted assets plus perp pnl minus',
							'spot liabilities, see `calculate_user_equity`). Below this the',
							'permissionless breaker can trip. Risk-increasing orders, fills,',
							'withdrawals and deposit transfers must clear `equity_floor +',
							'equity_floor_buffer`. Settable only by the warm/cold admin; 0 disables',
							'both checks.',
							'precision: QUOTE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'equityFloorBuffer';
						docs: [
							'Extra headroom above `equity_floor` required by risk-increasing',
							'actions, so an account cannot legally end an action at the trip',
							'threshold. No effect while `equity_floor` is 0.',
							'precision: QUOTE_PRECISION',
						];
						type: 'u64';
					},
				];
			};
		},
		{
			name: 'userFees';
			serialization: 'bytemuckunsafe';
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'totalFeePaid';
						docs: ['Total taker fee paid', 'precision: QUOTE_PRECISION'];
						type: 'u64';
					},
					{
						name: 'totalFeeRebate';
						docs: ['Total maker fee rebate', 'precision: QUOTE_PRECISION'];
						type: 'u64';
					},
					{
						name: 'totalTokenDiscount';
						docs: [
							'Total discount from holding token',
							'precision: QUOTE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'totalRefereeDiscount';
						docs: [
							'Total discount from being referred',
							'precision: QUOTE_PRECISION',
						];
						type: 'u64';
					},
				];
			};
		},
		{
			name: 'userStats';
			serialization: 'bytemuckunsafe';
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'authority';
						docs: ['The authority for all of a users sub accounts'];
						type: 'pubkey';
					},
					{
						name: 'referrer';
						docs: ['The address that referred this user'];
						type: 'pubkey';
					},
					{
						name: 'fees';
						docs: ['Stats on the fees paid by the user'];
						type: {
							defined: {
								name: 'userFees';
							};
						};
					},
					{
						name: 'makerVolume30d';
						docs: [
							'Rolling 30day maker volume for user',
							'precision: QUOTE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'takerVolume30d';
						docs: [
							'Rolling 30day taker volume for user',
							'precision: QUOTE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'fillerVolume30d';
						docs: [
							'Rolling 30day filler volume for user',
							'precision: QUOTE_PRECISION',
						];
						type: 'u64';
					},
					{
						name: 'lastMakerVolume30dTs';
						docs: ['last time the maker volume was updated'];
						type: 'i64';
					},
					{
						name: 'lastTakerVolume30dTs';
						docs: ['last time the taker volume was updated'];
						type: 'i64';
					},
					{
						name: 'lastFillerVolume30dTs';
						docs: ['last time the filler volume was updated'];
						type: 'i64';
					},
					{
						name: 'ifStakedQuoteAssetAmount';
						docs: ['The amount of tokens staked in the quote spot markets if'];
						type: 'u64';
					},
					{
						name: 'numberOfSubAccounts';
						docs: ['The current number of sub accounts'];
						type: 'u16';
					},
					{
						name: 'numberOfSubAccountsCreated';
						docs: [
							'The number of sub accounts created. Can be greater than the number of sub accounts if user',
							'has deleted sub accounts',
						];
						type: 'u16';
					},
					{
						name: 'referrerStatus';
						docs: [
							'Flags for referrer status:',
							'First bit (LSB): 1 if user is a referrer, 0 otherwise',
							'Second bit: 1 if user was referred, 0 otherwise',
						];
						type: 'u8';
					},
					{
						name: 'disableUpdatePerpBidAskTwap';
						type: 'u8';
					},
					{
						name: 'pausedOperations';
						type: 'u8';
					},
					{
						name: 'padding1';
						docs: [
							'9 bytes: 1 byte of former repr(C) alignment padding + the removed',
							'8-byte `if_staked_gov_token_amount` field (gov-token stake fee discount)',
						];
						type: {
							array: ['u8', 9];
						};
					},
					{
						name: 'delegatePermissions';
						docs: ['Delegate permissions across all sub accounts'];
						type: 'u8';
					},
					{
						name: 'equityBreakerTripped';
						docs: [
							'Set by the permissionless `trip_equity_floor_breaker` instruction when',
							"any of the authority's subaccounts falls below its equity floor.",
							'While set, every subaccount of the authority rejects risk-increasing',
							'fills, withdrawals and transfers out. Cleared only by the warm admin.',
						];
						type: 'u8';
					},
					{
						name: 'acceleratedReferralStatus';
						docs: [
							'Persistent referral reward status. See [`AcceleratedReferralStatus`]. Kept',
							'separate from `referrer_status`, which describes whether this authority',
							'refers or was referred by somebody else. Carved out of former padding so',
							'preupgrade accounts read `0` (standard, automatic enrollment allowed).',
						];
						type: 'u8';
					},
					{
						name: 'padding';
						type: {
							array: ['u8', 61];
						};
					},
				];
			};
		},
		{
			name: 'validityGuardRails';
			repr: {
				kind: 'c';
			};
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'slotsBeforeStaleForAmm';
						docs: [
							'Compact wall-clock duration encoded in historical 400ms slot quanta.',
						];
						type: 'i64';
					},
					{
						name: 'slotsBeforeStaleForMargin';
						docs: [
							'Compact wall-clock duration encoded in historical 400ms slot quanta.',
						];
						type: 'i64';
					},
					{
						name: 'confidenceIntervalMaxSize';
						type: 'u64';
					},
					{
						name: 'tooVolatileRatio';
						type: 'i64';
					},
				];
			};
		},
		{
			name: 'jit_proxy::state::MarketType';
			type: {
				kind: 'enum';
				variants: [
					{
						name: 'perp';
					},
					{
						name: 'spot';
					},
				];
			};
		},
		{
			name: 'velocity::state::user::MarketType';
			type: {
				kind: 'enum';
				variants: [
					{
						name: 'spot';
					},
					{
						name: 'perp';
					},
				];
			};
		},
	];
};
