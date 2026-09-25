/**
 * Program IDL in camelCase format in order to be used in JS/TS.
 *
 * Note that this is only a type helper and is not the actual IDL. The original
 * IDL can be found at `target/idl/protocol_revenue_router.json`.
 */
export type ProtocolRevenueRouter = {
	address: 'rout8Eh6aU911sDDyJaDWGY61mSfVhSNXqk1Bw9xeNn';
	metadata: {
		name: 'protocolRevenueRouter';
		version: '0.1.0';
		spec: '0.1.0';
		description: 'Routes Velocity perp protocol fees between the DFX recovery pool and the treasury';
	};
	instructions: [
		{
			name: 'distribute';
			discriminator: [191, 44, 223, 207, 164, 236, 126, 61];
			accounts: [
				{
					name: 'config';
					writable: true;
					pda: {
						seeds: [
							{
								kind: 'const';
								value: [
									114,
									111,
									117,
									116,
									101,
									114,
									95,
									99,
									111,
									110,
									102,
									105,
									103,
								];
							},
						];
					};
				},
				{
					name: 'cranker';
					signer: true;
				},
				{
					name: 'usdtMint';
				},
				{
					name: 'routerAta';
					writable: true;
					pda: {
						seeds: [
							{
								kind: 'account';
								path: 'config';
							},
							{
								kind: 'const';
								value: [
									6,
									221,
									246,
									225,
									215,
									101,
									161,
									147,
									217,
									203,
									225,
									70,
									206,
									235,
									121,
									172,
									28,
									180,
									133,
									237,
									95,
									91,
									55,
									145,
									58,
									140,
									245,
									133,
									126,
									255,
									0,
									169,
								];
							},
							{
								kind: 'account';
								path: 'usdtMint';
							},
						];
						program: {
							kind: 'const';
							value: [
								140,
								151,
								37,
								143,
								78,
								36,
								137,
								241,
								187,
								61,
								16,
								41,
								20,
								142,
								13,
								131,
								11,
								90,
								19,
								153,
								218,
								255,
								16,
								132,
								4,
								142,
								123,
								216,
								219,
								233,
								248,
								89,
							];
						};
					};
				},
				{
					name: 'treasury';
				},
				{
					name: 'treasuryAta';
					writable: true;
					pda: {
						seeds: [
							{
								kind: 'account';
								path: 'treasury';
							},
							{
								kind: 'const';
								value: [
									6,
									221,
									246,
									225,
									215,
									101,
									161,
									147,
									217,
									203,
									225,
									70,
									206,
									235,
									121,
									172,
									28,
									180,
									133,
									237,
									95,
									91,
									55,
									145,
									58,
									140,
									245,
									133,
									126,
									255,
									0,
									169,
								];
							},
							{
								kind: 'account';
								path: 'usdtMint';
							},
						];
						program: {
							kind: 'const';
							value: [
								140,
								151,
								37,
								143,
								78,
								36,
								137,
								241,
								187,
								61,
								16,
								41,
								20,
								142,
								13,
								131,
								11,
								90,
								19,
								153,
								218,
								255,
								16,
								132,
								4,
								142,
								123,
								216,
								219,
								233,
								248,
								89,
							];
						};
					};
				},
				{
					name: 'payer';
					writable: true;
					signer: true;
				},
				{
					name: 'redemptionConfig';
					writable: true;
					pda: {
						seeds: [
							{
								kind: 'const';
								value: [99, 111, 110, 102, 105, 103];
							},
						];
						program: {
							kind: 'const';
							value: [
								12,
								182,
								230,
								179,
								197,
								130,
								96,
								52,
								110,
								202,
								3,
								235,
								74,
								170,
								167,
								197,
								70,
								141,
								235,
								46,
								61,
								200,
								152,
								253,
								111,
								105,
								234,
								245,
								235,
								80,
								14,
								28,
							];
						};
					};
				},
				{
					name: 'redemptionLedger';
					writable: true;
					pda: {
						seeds: [
							{
								kind: 'const';
								value: [
									99,
									111,
									110,
									116,
									114,
									105,
									98,
									117,
									116,
									105,
									111,
									110,
									95,
									108,
									101,
									100,
									103,
									101,
									114,
								];
							},
						];
						program: {
							kind: 'const';
							value: [
								12,
								182,
								230,
								179,
								197,
								130,
								96,
								52,
								110,
								202,
								3,
								235,
								74,
								170,
								167,
								197,
								70,
								141,
								235,
								46,
								61,
								200,
								152,
								253,
								111,
								105,
								234,
								245,
								235,
								80,
								14,
								28,
							];
						};
					};
				},
				{
					name: 'redemptionVault';
					writable: true;
				},
				{
					name: 'dfxRedemptionProgram';
					address: 'rdemKHu2ueeKkhwmM2GfJFaqD3zsrj7s3oGMN3dQMJT';
				},
				{
					name: 'tokenProgram';
					address: 'TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA';
				},
				{
					name: 'associatedTokenProgram';
					address: 'ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL';
				},
				{
					name: 'systemProgram';
					address: '11111111111111111111111111111111';
				},
			];
			args: [];
		},
		{
			name: 'initialize';
			discriminator: [175, 175, 109, 31, 13, 152, 155, 237];
			accounts: [
				{
					name: 'config';
					writable: true;
					pda: {
						seeds: [
							{
								kind: 'const';
								value: [
									114,
									111,
									117,
									116,
									101,
									114,
									95,
									99,
									111,
									110,
									102,
									105,
									103,
								];
							},
						];
					};
				},
				{
					name: 'usdtMint';
				},
				{
					name: 'redemptionConfig';
					pda: {
						seeds: [
							{
								kind: 'const';
								value: [99, 111, 110, 102, 105, 103];
							},
						];
						program: {
							kind: 'const';
							value: [
								12,
								182,
								230,
								179,
								197,
								130,
								96,
								52,
								110,
								202,
								3,
								235,
								74,
								170,
								167,
								197,
								70,
								141,
								235,
								46,
								61,
								200,
								152,
								253,
								111,
								105,
								234,
								245,
								235,
								80,
								14,
								28,
							];
						};
					};
				},
				{
					name: 'admin';
				},
				{
					name: 'cranker';
				},
				{
					name: 'treasury';
				},
				{
					name: 'payer';
					writable: true;
					signer: true;
				},
				{
					name: 'systemProgram';
					address: '11111111111111111111111111111111';
				},
			];
			args: [
				{
					name: 'tiers';
					type: {
						vec: {
							defined: {
								name: 'tier';
							};
						};
					};
				},
			];
		},
		{
			name: 'updateConfig';
			discriminator: [29, 158, 252, 191, 10, 83, 219, 99];
			accounts: [
				{
					name: 'config';
					writable: true;
					pda: {
						seeds: [
							{
								kind: 'const';
								value: [
									114,
									111,
									117,
									116,
									101,
									114,
									95,
									99,
									111,
									110,
									102,
									105,
									103,
								];
							},
						];
					};
				},
				{
					name: 'admin';
					signer: true;
					relations: ['config'];
				},
				{
					name: 'redemptionConfig';
					pda: {
						seeds: [
							{
								kind: 'const';
								value: [99, 111, 110, 102, 105, 103];
							},
						];
						program: {
							kind: 'const';
							value: [
								12,
								182,
								230,
								179,
								197,
								130,
								96,
								52,
								110,
								202,
								3,
								235,
								74,
								170,
								167,
								197,
								70,
								141,
								235,
								46,
								61,
								200,
								152,
								253,
								111,
								105,
								234,
								245,
								235,
								80,
								14,
								28,
							];
						};
					};
				},
				{
					name: 'newAdmin';
					optional: true;
				},
				{
					name: 'newCranker';
					optional: true;
				},
				{
					name: 'newTreasury';
					optional: true;
				},
			];
			args: [
				{
					name: 'tiers';
					type: {
						option: {
							vec: {
								defined: {
									name: 'tier';
								};
							};
						};
					};
				},
			];
		},
	];
	accounts: [
		{
			name: 'routerConfig';
			discriminator: [147, 20, 81, 135, 46, 251, 46, 139];
		},
	];
	events: [
		{
			name: 'feesDistributed';
			discriminator: [209, 24, 174, 200, 236, 90, 154, 55];
		},
		{
			name: 'routerConfigUpdated';
			discriminator: [75, 159, 47, 177, 162, 44, 127, 69];
		},
		{
			name: 'routerInitialized';
			discriminator: [2, 194, 31, 191, 122, 98, 112, 243];
		},
	];
	errors: [
		{
			code: 6000;
			name: 'unauthorized';
			msg: 'Signer is not authorized for this instruction';
		},
		{
			code: 6001;
			name: 'invalidAuthority';
			msg: 'Authority key cannot be the default pubkey';
		},
		{
			code: 6002;
			name: 'emptyTiers';
			msg: 'At least one tier is required';
		},
		{
			code: 6003;
			name: 'tooManyTiers';
			msg: 'Too many tiers';
		},
		{
			code: 6004;
			name: 'firstTierThresholdNotZero';
			msg: 'The first tier must start at threshold 0';
		},
		{
			code: 6005;
			name: 'tiersNotIncreasing';
			msg: 'Tier thresholds must strictly increase';
		},
		{
			code: 6006;
			name: 'invalidBps';
			msg: 'Tier pool_bps must be at most 10000';
		},
		{
			code: 6007;
			name: 'tiersLockedForPeriod';
			msg: 'Tiers cannot change after a distribution in the current period';
		},
		{
			code: 6008;
			name: 'arithmeticOverflow';
			msg: 'Arithmetic overflow';
		},
		{
			code: 6009;
			name: 'invalidTreasury';
			msg: 'Treasury cannot be the default key, the router config or the redemption config';
		},
		{
			code: 6010;
			name: 'usdtMintMismatch';
			msg: "USDT mint does not match the redemption config's mint";
		},
	];
	types: [
		{
			name: 'config';
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'bump';
						type: 'u8';
					},
					{
						name: 'admin';
						type: 'pubkey';
					},
					{
						name: 'dfxMint';
						type: 'pubkey';
					},
					{
						name: 'usdtMint';
						type: 'pubkey';
					},
					{
						name: 'usdtVault';
						type: 'pubkey';
					},
					{
						name: 'totalExploitedAmount';
						type: 'u64';
					},
					{
						name: 'redemptionThreshold';
						type: 'u64';
					},
					{
						name: 'lifetimeRecognizedBacking';
						type: 'u64';
					},
					{
						name: 'recognizedBackingRemaining';
						type: 'u64';
					},
					{
						name: 'totalRedeemedUsdt';
						type: 'u64';
					},
					{
						name: 'redemptionStarted';
						type: 'bool';
					},
					{
						name: 'paused';
						type: 'bool';
					},
				];
			};
		},
		{
			name: 'contributionLedger';
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'bump';
						type: 'u8';
					},
					{
						name: 'depositors';
						type: {
							array: [
								{
									defined: {
										name: 'depositor';
									};
								},
								6,
							];
						};
					},
					{
						name: 'routerProgram';
						type: 'pubkey';
					},
					{
						name: 'routerConfig';
						type: 'pubkey';
					},
					{
						name: 'cumulativeProtocolFees';
						type: 'u64';
					},
					{
						name: 'cumulativeTether';
						type: 'u64';
					},
					{
						name: 'cumulativeSeedCapital';
						type: 'u64';
					},
					{
						name: 'cumulativeOther';
						type: 'u64';
					},
					{
						name: 'reserved';
						type: {
							array: ['u8', 96];
						};
					},
				];
			};
		},
		{
			name: 'depositor';
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'key';
						type: 'pubkey';
					},
					{
						name: 'allowedSources';
						type: 'u8';
					},
				];
			};
		},
		{
			name: 'feesDistributed';
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'ts';
						type: 'i64';
					},
					{
						name: 'total';
						type: 'u64';
					},
					{
						name: 'toPool';
						type: 'u64';
					},
					{
						name: 'toTreasury';
						type: 'u64';
					},
					{
						name: 'capRoomAfter';
						type: 'u64';
					},
					{
						name: 'periodDay';
						type: 'i64';
					},
					{
						name: 'periodFeesAfter';
						type: 'u64';
					},
					{
						name: 'lifetimeFeesAfter';
						type: 'u64';
					},
				];
			};
		},
		{
			name: 'routerConfig';
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'bump';
						type: 'u8';
					},
					{
						name: 'admin';
						type: 'pubkey';
					},
					{
						name: 'cranker';
						type: 'pubkey';
					},
					{
						name: 'usdtMint';
						type: 'pubkey';
					},
					{
						name: 'treasury';
						type: 'pubkey';
					},
					{
						name: 'tiers';
						type: {
							array: [
								{
									defined: {
										name: 'tier';
									};
								},
								8,
							];
						};
					},
					{
						name: 'tierCount';
						type: 'u8';
					},
					{
						name: 'periodDay';
						type: 'i64';
					},
					{
						name: 'periodFees';
						type: 'u64';
					},
					{
						name: 'lifetimeFees';
						type: 'u64';
					},
					{
						name: 'lifetimeToPool';
						type: 'u64';
					},
					{
						name: 'lifetimeToTreasury';
						type: 'u64';
					},
					{
						name: 'reserved';
						type: {
							array: ['u8', 128];
						};
					},
				];
			};
		},
		{
			name: 'routerConfigUpdated';
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'ts';
						type: 'i64';
					},
					{
						name: 'admin';
						type: 'pubkey';
					},
					{
						name: 'cranker';
						type: 'pubkey';
					},
					{
						name: 'treasury';
						type: 'pubkey';
					},
					{
						name: 'tierCount';
						type: 'u8';
					},
				];
			};
		},
		{
			name: 'routerInitialized';
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'ts';
						type: 'i64';
					},
					{
						name: 'admin';
						type: 'pubkey';
					},
					{
						name: 'cranker';
						type: 'pubkey';
					},
					{
						name: 'treasury';
						type: 'pubkey';
					},
					{
						name: 'usdtMint';
						type: 'pubkey';
					},
					{
						name: 'tierCount';
						type: 'u8';
					},
				];
			};
		},
		{
			name: 'tier';
			docs: [
				'One slice of the marginal ladder: gross from `threshold` up to the next',
				"tier's threshold is split `pool_bps` to the recovery pool.",
			];
			type: {
				kind: 'struct';
				fields: [
					{
						name: 'threshold';
						type: 'u64';
					},
					{
						name: 'poolBps';
						type: 'u16';
					},
				];
			};
		},
	];
};
