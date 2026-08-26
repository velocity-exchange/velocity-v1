/**
 * Program IDL in camelCase format in order to be used in JS/TS.
 *
 * Note that this is only a type helper and is not the actual IDL. The original
 * IDL can be found at `target/idl/velocity.json`.
 */
export type Velocity = {
  "address": "vELoC1audYbSYVRXn1vPaV8Axoa9oU6BYmNGZZBDZ1P",
  "metadata": {
    "name": "velocity",
    "version": "2.165.0",
    "spec": "0.1.0",
    "description": "Created with Anchor"
  },
  "instructions": [
    {
      "name": "addAmmConstituentMappingData",
      "discriminator": [
        164,
        236,
        130,
        40,
        118,
        179,
        46,
        235
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "lpPool"
        },
        {
          "name": "ammConstituentMapping",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  65,
                  77,
                  77,
                  95,
                  77,
                  65,
                  80
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "constituentTargetBase",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  111,
                  110,
                  115,
                  116,
                  105,
                  116,
                  117,
                  101,
                  110,
                  116,
                  95,
                  116,
                  97,
                  114,
                  103,
                  101,
                  116,
                  95,
                  98,
                  97,
                  115,
                  101,
                  95,
                  115,
                  101,
                  101,
                  100
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "state"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "ammConstituentMappingData",
          "type": {
            "vec": {
              "defined": {
                "name": "addAmmConstituentMappingDatum"
              }
            }
          }
        }
      ]
    },
    {
      "name": "addInsuranceFundStake",
      "discriminator": [
        251,
        144,
        115,
        11,
        222,
        47,
        62,
        236
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "insuranceFundStake",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "insuranceFundStake",
            "userStats"
          ]
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "insuranceFundVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  105,
                  110,
                  115,
                  117,
                  114,
                  97,
                  110,
                  99,
                  101,
                  95,
                  102,
                  117,
                  110,
                  100,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "userTokenAccount",
          "writable": true
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "addMarketToAmmCache",
      "discriminator": [
        112,
        149,
        195,
        222,
        124,
        7,
        87,
        237
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "ammCache",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  97,
                  109,
                  109,
                  95,
                  99,
                  97,
                  99,
                  104,
                  101,
                  95,
                  115,
                  101,
                  101,
                  100
                ]
              }
            ]
          }
        },
        {
          "name": "perpMarket"
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": []
    },
    {
      "name": "adminDeposit",
      "discriminator": [
        210,
        66,
        65,
        182,
        102,
        214,
        176,
        30
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "adminTokenAccount",
          "writable": true
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "adminUpdateUserStatsPausedOperations",
      "discriminator": [
        183,
        104,
        63,
        150,
        240,
        199,
        3,
        10
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "userStats",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "pausedOperations",
          "type": "u8"
        }
      ]
    },
    {
      "name": "beginLpSwap",
      "discriminator": [
        64,
        44,
        24,
        199,
        48,
        125,
        67,
        91
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "signerOutTokenAccount",
          "docs": [
            "Signer token accounts"
          ],
          "writable": true
        },
        {
          "name": "signerInTokenAccount",
          "writable": true
        },
        {
          "name": "constituentOutTokenAccount",
          "docs": [
            "Constituent token accounts"
          ],
          "writable": true
        },
        {
          "name": "constituentInTokenAccount",
          "writable": true
        },
        {
          "name": "outConstituent",
          "docs": [
            "Constituents"
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "outMarketIndex"
              }
            ]
          }
        },
        {
          "name": "inConstituent",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "inMarketIndex"
              }
            ]
          }
        },
        {
          "name": "lpPool"
        },
        {
          "name": "instructions",
          "docs": [
            "Instructions Sysvar for instruction introspection"
          ],
          "address": "Sysvar1nstructions1111111111111111111111111"
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "inMarketIndex",
          "type": "u16"
        },
        {
          "name": "outMarketIndex",
          "type": "u16"
        },
        {
          "name": "amountIn",
          "type": "u64"
        }
      ]
    },
    {
      "name": "beginSwap",
      "discriminator": [
        174,
        109,
        228,
        1,
        242,
        105,
        232,
        105
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "outSpotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "outMarketIndex"
              }
            ]
          }
        },
        {
          "name": "inSpotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "inMarketIndex"
              }
            ]
          }
        },
        {
          "name": "outTokenAccount",
          "writable": true
        },
        {
          "name": "inTokenAccount",
          "writable": true
        },
        {
          "name": "tokenProgram"
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "instructions",
          "docs": [
            "Instructions Sysvar for instruction introspection"
          ],
          "address": "Sysvar1nstructions1111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "inMarketIndex",
          "type": "u16"
        },
        {
          "name": "outMarketIndex",
          "type": "u16"
        },
        {
          "name": "amountIn",
          "type": "u64"
        }
      ]
    },
    {
      "name": "cancelAllClobOrders",
      "docs": [
        "Pull every resting CLOB order this `User` holds on one side (or both) in",
        "a single CPI, unwinding the aggregates from per-side totals. The book",
        "caps one sweep; the log says when it stopped early and the call is safe",
        "to repeat."
      ],
      "discriminator": [
        161,
        193,
        25,
        181,
        238,
        63,
        127,
        144
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "quoter"
        },
        {
          "name": "clobMarket",
          "docs": [
            "accounts in the handler."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "set to. Deliberately not the vault authority: signer privilege is",
            "inherited by a callee, so the key velocity hands an external program",
            "must be the authority on nothing."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "Wake-hint host; optional like every other CLOB path. Pulling orders can",
            "only *relax* the expiry and activation hints, so a caller that omits it",
            "leaves the cranks waking earlier than they need to — latency, not",
            "liveness."
          ],
          "writable": true,
          "optional": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "arg",
                "path": "params.market_index"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "cancelAllClobOrdersParams"
            }
          }
        }
      ]
    },
    {
      "name": "cancelClobOrder",
      "discriminator": [
        145,
        107,
        104,
        232,
        171,
        3,
        245,
        94
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "perpMarket",
          "docs": [
            "Read-only, and read for one thing: the cached oracle price the cancel",
            "record is stamped with. Deliberately not an oracle account — a maker",
            "pulling orders off a book must not be able to fail on a stale feed."
          ]
        },
        {
          "name": "quoter"
        },
        {
          "name": "clobMarket",
          "docs": [
            "accounts in the handler."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "set to. Deliberately not the vault authority: signer privilege is",
            "inherited by a callee, so the key velocity hands an external program",
            "must be the authority on nothing."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "cancelClobOrderParams"
            }
          }
        }
      ]
    },
    {
      "name": "cancelOrder",
      "discriminator": [
        95,
        129,
        237,
        240,
        8,
        49,
        223,
        132
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "orderId",
          "type": {
            "option": "u32"
          }
        }
      ]
    },
    {
      "name": "cancelOrderByUserId",
      "discriminator": [
        107,
        211,
        250,
        133,
        18,
        37,
        57,
        100
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "userOrderId",
          "type": "u8"
        }
      ]
    },
    {
      "name": "cancelOrders",
      "discriminator": [
        238,
        225,
        95,
        158,
        227,
        103,
        8,
        194
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "marketType",
          "type": {
            "option": {
              "defined": {
                "name": "marketType"
              }
            }
          }
        },
        {
          "name": "marketIndex",
          "type": {
            "option": "u16"
          }
        },
        {
          "name": "direction",
          "type": {
            "option": {
              "defined": {
                "name": "positionDirection"
              }
            }
          }
        }
      ]
    },
    {
      "name": "cancelOrdersByIds",
      "discriminator": [
        134,
        19,
        144,
        165,
        94,
        240,
        210,
        94
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "orderIds",
          "type": {
            "vec": "u32"
          }
        }
      ]
    },
    {
      "name": "cancelRequestRemoveInsuranceFundStake",
      "discriminator": [
        97,
        235,
        78,
        62,
        212,
        42,
        241,
        127
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "insuranceFundStake",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "insuranceFundStake",
            "userStats"
          ]
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "insuranceFundVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  105,
                  110,
                  115,
                  117,
                  114,
                  97,
                  110,
                  99,
                  101,
                  95,
                  102,
                  117,
                  110,
                  100,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "changeApprovedBuilder",
      "discriminator": [
        179,
        134,
        211,
        45,
        195,
        5,
        189,
        173
      ],
      "accounts": [
        {
          "name": "escrow",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  82,
                  69,
                  86,
                  95,
                  69,
                  83,
                  67,
                  82,
                  79,
                  87
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "escrow"
          ]
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "builder",
          "type": "pubkey"
        },
        {
          "name": "maxFeeBps",
          "type": "u16"
        },
        {
          "name": "add",
          "type": "bool"
        }
      ]
    },
    {
      "name": "changeSignedMsgWsDelegateStatus",
      "discriminator": [
        252,
        202,
        252,
        219,
        179,
        27,
        84,
        138
      ],
      "accounts": [
        {
          "name": "signedMsgWsDelegates",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  83,
                  73,
                  71,
                  78,
                  69,
                  68,
                  95,
                  77,
                  83,
                  71,
                  95,
                  87,
                  83
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        },
        {
          "name": "authority",
          "writable": true,
          "signer": true
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "delegate",
          "type": "pubkey"
        },
        {
          "name": "add",
          "type": "bool"
        }
      ]
    },
    {
      "name": "crankClobEvict",
      "discriminator": [
        151,
        166,
        191,
        4,
        122,
        28,
        36,
        220
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "docs": [
            "the constraint below); in program-keeper mode it is only the lamport",
            "payout target — relay's `KEEPER_PLACEHOLDER` slot — and no signature",
            "is required."
          ],
          "writable": true
        },
        {
          "name": "filler",
          "writable": true
        },
        {
          "name": "fillerStats",
          "writable": true
        },
        {
          "name": "user",
          "docs": [
            "The owner of the order being removed (the book's tail for evict, the",
            "hinted order for expire). Verified against the CLOB's return data —",
            "a race that removed someone else's order fails the whole crank."
          ],
          "writable": true
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "quoter",
          "docs": [
            "Deliberately not gated on active/approved: dead books still need",
            "their resting orders reclaimed."
          ]
        },
        {
          "name": "clobMarket",
          "docs": [
            "accounts in the handler."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "set to. Deliberately not the vault authority: signer privilege is",
            "inherited by a callee, so the key velocity hands an external program",
            "must be the authority on nothing."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "The market's relay conditions account: the expiry-hint host and the",
            "lamport reservoir. Optional so signed keepers can crank markets whose",
            "conditions were never initialized; required in program-keeper mode."
          ],
          "writable": true,
          "optional": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "side",
          "type": {
            "defined": {
              "name": "sideV0"
            }
          }
        }
      ]
    },
    {
      "name": "crankClobRemoveExpired",
      "discriminator": [
        35,
        80,
        27,
        105,
        148,
        23,
        65,
        181
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "docs": [
            "the constraint below); in program-keeper mode it is only the lamport",
            "payout target — relay's `KEEPER_PLACEHOLDER` slot — and no signature",
            "is required."
          ],
          "writable": true
        },
        {
          "name": "filler",
          "writable": true
        },
        {
          "name": "fillerStats",
          "writable": true
        },
        {
          "name": "user",
          "docs": [
            "The owner of the order being removed (the book's tail for evict, the",
            "hinted order for expire). Verified against the CLOB's return data —",
            "a race that removed someone else's order fails the whole crank."
          ],
          "writable": true
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "quoter",
          "docs": [
            "Deliberately not gated on active/approved: dead books still need",
            "their resting orders reclaimed."
          ]
        },
        {
          "name": "clobMarket",
          "docs": [
            "accounts in the handler."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "set to. Deliberately not the vault authority: signer privilege is",
            "inherited by a callee, so the key velocity hands an external program",
            "must be the authority on nothing."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "The market's relay conditions account: the expiry-hint host and the",
            "lamport reservoir. Optional so signed keepers can crank markets whose",
            "conditions were never initialized; required in program-keeper mode."
          ],
          "writable": true,
          "optional": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "orderRef",
          "type": {
            "defined": {
              "name": "clobOrderRefV0"
            }
          }
        }
      ]
    },
    {
      "name": "crankCrossMatch",
      "docs": [
        "Fill two crossed resting sources against each other (permissionless;",
        "the protocol User takes both legs and keeps the spread, the caller is",
        "paid reservoir lamports). Reverts unless profitable after fees."
      ],
      "discriminator": [
        121,
        104,
        3,
        82,
        220,
        85,
        74,
        57
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "docs": [
            "No signature: the executor's own profitability predicate is the gate."
          ],
          "writable": true
        },
        {
          "name": "taker",
          "docs": [
            "The protocol-owned pass-through taker. Locked to the protocol `User`",
            "so the reservoir never pays for someone else's private arb."
          ],
          "writable": true
        },
        {
          "name": "takerStats",
          "writable": true
        },
        {
          "name": "crankConditions",
          "docs": [
            "The market's conditions account: the reservoir that pays the keeper."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "size",
          "type": "u64"
        },
        {
          "name": "buyQuoterIndex",
          "type": "u8"
        },
        {
          "name": "sellQuoterIndex",
          "type": "u8"
        }
      ]
    },
    {
      "name": "crankTakerOriginCross",
      "docs": [
        "Resolve one taker-origin cross on a market's CLOB (permissionless):",
        "consume the crossing counterparty at its own price, lift the migrated",
        "taker remainder off the book, and settle the pair at the counterparty's",
        "price so the taker — not whoever lands a transaction at the activation",
        "slot — captures the improvement. The cranker is paid a filler reward in",
        "quote out of that improvement, capped so the taker's net still beats the",
        "price it was resting at; a cross that cannot clear that bar is left",
        "resting."
      ],
      "discriminator": [
        105,
        245,
        226,
        50,
        241,
        206,
        172,
        254
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "docs": [
            "constraint below enforces it); in program-keeper mode it is only the",
            "lamport payout target — relay's keeper-placeholder slot — and no",
            "signature is required."
          ],
          "writable": true
        },
        {
          "name": "filler",
          "docs": [
            "The cranker's margin account: the crank reward lands here as quote."
          ],
          "writable": true
        },
        {
          "name": "fillerStats",
          "writable": true
        },
        {
          "name": "taker",
          "docs": [
            "Owner of the taker-origin order — the taker of this match. Verified",
            "against the identity the CLOB reports on removal, so a wrong account",
            "fails the crank rather than settling against someone else."
          ],
          "writable": true
        },
        {
          "name": "takerStats",
          "writable": true
        },
        {
          "name": "quoter",
          "docs": [
            "The market's CLOB registry entry."
          ]
        },
        {
          "name": "clobMarket",
          "docs": [
            "(`ClobMarket::from_quoter`), so a valid entry cannot be pointed at an",
            "arbitrary account."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "set to, and the authority on nothing else."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "The market's relay conditions account: the wake-hint host and the",
            "lamport reservoir. Optional so a signed keeper can crank a market whose",
            "conditions were never initialized; required in program-keeper mode."
          ],
          "writable": true,
          "optional": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "deleteAmmCache",
      "discriminator": [
        216,
        130,
        215,
        206,
        233,
        232,
        191,
        88
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "ammCache",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  97,
                  109,
                  109,
                  95,
                  99,
                  97,
                  99,
                  104,
                  101,
                  95,
                  115,
                  101,
                  101,
                  100
                ]
              }
            ]
          }
        }
      ],
      "args": []
    },
    {
      "name": "deleteInitializedPerpMarket",
      "discriminator": [
        91,
        154,
        24,
        87,
        106,
        59,
        190,
        66
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "deleteInitializedSpotMarket",
      "discriminator": [
        31,
        140,
        67,
        191,
        189,
        20,
        101,
        221
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "spotMarket",
          "writable": true
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "insuranceFundVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  105,
                  110,
                  115,
                  117,
                  114,
                  97,
                  110,
                  99,
                  101,
                  95,
                  102,
                  117,
                  110,
                  100,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "deletePrelaunchOracle",
      "discriminator": [
        59,
        169,
        100,
        49,
        69,
        17,
        173,
        253
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "prelaunchOracle",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  112,
                  114,
                  101,
                  108,
                  97,
                  117,
                  110,
                  99,
                  104,
                  95,
                  111,
                  114,
                  97,
                  99,
                  108,
                  101
                ]
              },
              {
                "kind": "arg",
                "path": "perpMarketIndex"
              }
            ]
          }
        },
        {
          "name": "perpMarket"
        },
        {
          "name": "state"
        }
      ],
      "args": [
        {
          "name": "perpMarketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "deleteSignedMsgUserOrders",
      "discriminator": [
        221,
        247,
        128,
        253,
        212,
        254,
        46,
        153
      ],
      "accounts": [
        {
          "name": "signedMsgUserOrders",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  83,
                  73,
                  71,
                  78,
                  69,
                  68,
                  95,
                  77,
                  83,
                  71
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        },
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": []
    },
    {
      "name": "deleteUser",
      "discriminator": [
        186,
        85,
        17,
        249,
        219,
        231,
        98,
        251
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "authority",
          "writable": true,
          "signer": true,
          "relations": [
            "user",
            "userStats"
          ]
        },
        {
          "name": "revenueShareEscrow",
          "docs": [
            "most users never create one. Deliberately an `UncheckedAccount` **pinned by",
            "`seeds`** rather than a typed `AccountLoader`: because the address is derived",
            "and not caller-chosen, absence is *provable* (`data_is_empty()`), so the handler",
            "can distinguish \"this authority has no escrow\" from \"the caller omitted it to",
            "skip the check\". A typed loader would instead make deletion impossible for the",
            "majority of users, who have no escrow account to pass.",
            "",
            "Required rather than `Option` so a caller holding fee-bearing builder rows",
            "cannot simply leave it out (OtterSec #128)."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  82,
                  69,
                  86,
                  95,
                  69,
                  83,
                  67,
                  82,
                  79,
                  87
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        }
      ],
      "args": []
    },
    {
      "name": "deposit",
      "discriminator": [
        242,
        35,
        198,
        137,
        82,
        225,
        242,
        182
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "userTokenAccount",
          "writable": true
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "u64"
        },
        {
          "name": "reduceOnly",
          "type": "bool"
        }
      ]
    },
    {
      "name": "depositIntoIsolatedPerpPosition",
      "discriminator": [
        101,
        48,
        255,
        153,
        127,
        121,
        170,
        26
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "spotMarketIndex"
              }
            ]
          }
        },
        {
          "name": "userTokenAccount",
          "writable": true
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "spotMarketIndex",
          "type": "u16"
        },
        {
          "name": "perpMarketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "depositIntoPerpMarketFeePool",
      "discriminator": [
        34,
        58,
        57,
        68,
        97,
        80,
        244,
        6
      ],
      "accounts": [
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "sourceVault",
          "writable": true
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "quoteSpotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "depositIntoSpotMarketRevenuePool",
      "discriminator": [
        92,
        40,
        151,
        42,
        122,
        254,
        139,
        246
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        },
        {
          "name": "authority",
          "writable": true,
          "signer": true
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "spotMarket"
              }
            ]
          }
        },
        {
          "name": "userTokenAccount",
          "writable": true
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "depositIntoSpotMarketVault",
      "discriminator": [
        48,
        252,
        119,
        73,
        255,
        205,
        174,
        247
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        },
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "sourceVault",
          "writable": true
        },
        {
          "name": "spotMarketVault",
          "writable": true
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "depositToProgramVault",
      "discriminator": [
        235,
        171,
        121,
        80,
        57,
        239,
        147,
        220
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "constituent",
          "writable": true
        },
        {
          "name": "constituentTokenAccount",
          "writable": true
        },
        {
          "name": "spotMarket",
          "writable": true
        },
        {
          "name": "spotMarketVault",
          "writable": true
        },
        {
          "name": "tokenProgram"
        },
        {
          "name": "mint"
        },
        {
          "name": "oracle"
        }
      ],
      "args": [
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "endLpSwap",
      "discriminator": [
        99,
        125,
        214,
        165,
        129,
        175,
        253,
        135
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "signerOutTokenAccount",
          "docs": [
            "Signer token accounts"
          ],
          "writable": true
        },
        {
          "name": "signerInTokenAccount",
          "writable": true
        },
        {
          "name": "constituentOutTokenAccount",
          "docs": [
            "Constituent token accounts"
          ],
          "writable": true
        },
        {
          "name": "constituentInTokenAccount",
          "writable": true
        },
        {
          "name": "outConstituent",
          "docs": [
            "Constituents"
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "outMarketIndex"
              }
            ]
          }
        },
        {
          "name": "inConstituent",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "inMarketIndex"
              }
            ]
          }
        },
        {
          "name": "lpPool"
        },
        {
          "name": "instructions",
          "docs": [
            "Instructions Sysvar for instruction introspection"
          ],
          "address": "Sysvar1nstructions1111111111111111111111111"
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "inMarketIndex",
          "type": "u16"
        },
        {
          "name": "outMarketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "endSwap",
      "discriminator": [
        177,
        184,
        27,
        193,
        34,
        13,
        210,
        145
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "outSpotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "outMarketIndex"
              }
            ]
          }
        },
        {
          "name": "inSpotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "inMarketIndex"
              }
            ]
          }
        },
        {
          "name": "outTokenAccount",
          "writable": true
        },
        {
          "name": "inTokenAccount",
          "writable": true
        },
        {
          "name": "tokenProgram"
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "instructions",
          "docs": [
            "Instructions Sysvar for instruction introspection"
          ],
          "address": "Sysvar1nstructions1111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "inMarketIndex",
          "type": "u16"
        },
        {
          "name": "outMarketIndex",
          "type": "u16"
        },
        {
          "name": "limitPrice",
          "type": {
            "option": "u64"
          }
        },
        {
          "name": "reduceOnly",
          "type": {
            "option": {
              "defined": {
                "name": "swapReduceOnly"
              }
            }
          }
        }
      ]
    },
    {
      "name": "extendAccount",
      "discriminator": [
        234,
        102,
        194,
        203,
        150,
        72,
        62,
        229
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "account",
          "docs": [
            "size) from the account discriminator"
          ],
          "writable": true
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": []
    },
    {
      "name": "extendAccountDevnet",
      "docs": [
        "Devnet/test-only: grow a zero-copy account to an arbitrary larger size",
        "to exercise the extension flow before a real struct extension exists.",
        "Stripped from production mainnet builds; `anchor-test` keeps it so the",
        "integration suite (which builds with default features + `anchor-test`)",
        "can exercise extension end to end."
      ],
      "discriminator": [
        58,
        206,
        231,
        21,
        136,
        141,
        180,
        252
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "account",
          "docs": [
            "zero-copy discriminator"
          ],
          "writable": true
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "newLen",
          "type": "u64"
        }
      ]
    },
    {
      "name": "fillPerpOrder",
      "docs": [
        "`signed_route` is the route the order's signer chose, as the filler",
        "read it off their signed message. It is checked against the digest the",
        "order carries, so a filler cannot misreport it, and every entry in it",
        "must appear in this transaction — the taker picks who competes for",
        "their flow, not the filler. Empty for an order with no signed route."
      ],
      "discriminator": [
        13,
        188,
        248,
        103,
        134,
        217,
        106,
        240
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "filler",
          "writable": true
        },
        {
          "name": "fillerStats",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "orderId",
          "type": {
            "option": "u32"
          }
        },
        {
          "name": "makerOrderId",
          "type": {
            "option": "u32"
          }
        },
        {
          "name": "signedRoute",
          "type": {
            "vec": "pubkey"
          }
        }
      ]
    },
    {
      "name": "fillPerpOrderV1",
      "docs": [
        "`fill_perp_order` with the market's CLOB accounts required: a restable",
        "remainder of the filled order migrates to the book instead of resting",
        "in `User.orders`. v0's account list is frozen, so this is a separate",
        "endpoint. `market_index` is an argument because the crank-conditions",
        "PDA seed needs it before any account is loaded; it is checked against",
        "the order's own market."
      ],
      "discriminator": [
        88,
        149,
        73,
        149,
        110,
        236,
        243,
        188
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "filler",
          "writable": true
        },
        {
          "name": "fillerStats",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "quoter",
          "docs": [
            "The market's CLOB registry entry — a remainder only ever rests on a",
            "vetted book."
          ]
        },
        {
          "name": "clobMarket",
          "docs": [
            "(`ClobMarket::from_quoter`), so a valid entry cannot be pointed at an",
            "arbitrary account."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "and the authority on nothing else."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "Wake-hint host for the rested remainder. Optional as on every CLOB",
            "placement path: a market whose conditions were never initialized must",
            "still be fillable, and a missed hint costs crank latency, not liveness."
          ],
          "writable": true,
          "optional": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "orderId",
          "type": {
            "option": "u32"
          }
        },
        {
          "name": "makerOrderId",
          "type": {
            "option": "u32"
          }
        },
        {
          "name": "signedRoute",
          "type": {
            "vec": "pubkey"
          }
        },
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "forceCancelClobOrders",
      "docs": [
        "Force-cancel a failing account's CLOB orders (keeper-passed",
        "`OrderRef`s; same gates and flat fee as `force_cancel_orders`)."
      ],
      "discriminator": [
        4,
        155,
        214,
        86,
        4,
        110,
        182,
        30
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "docs": [
            "program-keeper mode (protocol `User` as filler, relay turners) it is",
            "only the reservoir payout target and no signature is required."
          ],
          "writable": true
        },
        {
          "name": "filler",
          "writable": true
        },
        {
          "name": "fillerStats",
          "writable": true
        },
        {
          "name": "user",
          "docs": [
            "The deteriorated account whose CLOB orders are being reclaimed."
          ],
          "writable": true
        },
        {
          "name": "userStats",
          "docs": [
            "Carries the authority-wide equity breaker, which is grounds on its own."
          ]
        },
        {
          "name": "quoter",
          "docs": [
            "Deliberately not gated on active/approved: dead books still need",
            "failing makers' orders reclaimed."
          ]
        },
        {
          "name": "clobMarket",
          "docs": [
            "accounts in the handler."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "set to. Deliberately not the vault authority: signer privilege is",
            "inherited by a callee, so the key velocity hands an external program",
            "must be the authority on nothing."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "Wake-hint host; optional like every other CLOB path."
          ],
          "writable": true,
          "optional": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "orderRefs",
          "type": {
            "vec": {
              "defined": {
                "name": "forceCancelClobRefV0"
              }
            }
          }
        }
      ]
    },
    {
      "name": "forceCancelOrders",
      "discriminator": [
        64,
        181,
        196,
        63,
        222,
        72,
        64,
        232
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "filler",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        }
      ],
      "args": []
    },
    {
      "name": "forceDeleteUser",
      "discriminator": [
        2,
        241,
        195,
        172,
        227,
        24,
        254,
        158
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "authority",
          "writable": true,
          "relations": [
            "user",
            "userStats"
          ]
        },
        {
          "name": "keeper",
          "writable": true,
          "signer": true
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "revenueShareEscrow",
          "docs": [
            "because most users never create one. It carries the same contract as",
            "`DeleteUser::revenue_share_escrow`: an `UncheckedAccount` pinned by `seeds`, so",
            "the handler can tell \"this authority has no escrow\" (`data_is_empty()`) from \"the",
            "keeper omitted the account to skip the check\". It is required rather than",
            "`Option` for that second reason (OtterSec #128)."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  82,
                  69,
                  86,
                  95,
                  69,
                  83,
                  67,
                  82,
                  79,
                  87
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        }
      ],
      "args": []
    },
    {
      "name": "forceWipeAccountsDevnet",
      "docs": [
        "Devnet-only escape hatch: cleans up accounts stranded by a layout-breaking",
        "program upgrade (or by a partial re-init). For each account passed via",
        "`remaining_accounts`:",
        "- velocity-owned PDA → drain lamports (runtime GCs at end of tx)",
        "- token-program owned vault (velocity_signer close-authority) → CPI",
        "`close_account`, rent refunded to admin",
        "Admin gate reads State's first pubkey field at raw offset 8..40 so it",
        "works regardless of the State layout currently on chain. `velocity_signer_nonce`",
        "must match `State.signer_nonce`; mismatch fails the token CPI signature.",
        "Stripped from mainnet builds via `mainnet-beta`."
      ],
      "discriminator": [
        105,
        74,
        87,
        6,
        166,
        227,
        138,
        215
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state",
          "docs": [
            "(cold-)admin pubkey at offset 8..40."
          ]
        },
        {
          "name": "velocitySigner",
          "docs": [
            "at CPI time when closing token vaults; ignored otherwise."
          ]
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "velocitySignerNonce",
          "type": "u8"
        }
      ]
    },
    {
      "name": "forfeitRevenueShareOrder",
      "discriminator": [
        141,
        205,
        148,
        171,
        116,
        92,
        53,
        250
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  112,
                  101,
                  114,
                  112,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "spotMarket",
          "docs": [
            "The quote spot market of the perp market. The PDA seeds enforce this. The handler values",
            "the pnl pool against it. It is writable because the handler accrues interest first."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "perpMarket"
              }
            ]
          }
        },
        {
          "name": "escrowAuthority",
          "docs": [
            "The owner of the escrow that holds the row."
          ]
        },
        {
          "name": "revenueShareEscrow",
          "docs": [
            "The escrow that holds the row to write off."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  82,
                  69,
                  86,
                  95,
                  69,
                  83,
                  67,
                  82,
                  79,
                  87
                ]
              },
              {
                "kind": "account",
                "path": "escrowAuthority"
              }
            ]
          }
        },
        {
          "name": "beneficiaryUser",
          "docs": [
            "Sub-account 0 of the beneficiary of the row. This is the payout account. The handler proves",
            "that it does not exist."
          ]
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "orderIndex",
          "type": "u32"
        }
      ]
    },
    {
      "name": "initialize",
      "discriminator": [
        175,
        175,
        109,
        31,
        13,
        152,
        155,
        237
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
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
                  101
                ]
              }
            ]
          }
        },
        {
          "name": "quoteAssetMint"
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": []
    },
    {
      "name": "initializeAmmCache",
      "discriminator": [
        38,
        60,
        171,
        158,
        203,
        58,
        137,
        8
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "ammCache",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  97,
                  109,
                  109,
                  95,
                  99,
                  97,
                  99,
                  104,
                  101,
                  95,
                  115,
                  101,
                  101,
                  100
                ]
              }
            ]
          }
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": []
    },
    {
      "name": "initializeConstituent",
      "discriminator": [
        12,
        196,
        45,
        218,
        93,
        89,
        0,
        33
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "lpPool",
          "writable": true
        },
        {
          "name": "constituentTargetBase",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  111,
                  110,
                  115,
                  116,
                  105,
                  116,
                  117,
                  101,
                  110,
                  116,
                  95,
                  116,
                  97,
                  114,
                  103,
                  101,
                  116,
                  95,
                  98,
                  97,
                  115,
                  101,
                  95,
                  115,
                  101,
                  101,
                  100
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "constituentCorrelations",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  111,
                  110,
                  115,
                  116,
                  105,
                  116,
                  117,
                  101,
                  110,
                  116,
                  95,
                  99,
                  111,
                  114,
                  114,
                  101,
                  108,
                  97,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "constituent",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "spotMarketIndex"
              }
            ]
          }
        },
        {
          "name": "spotMarket",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "spotMarketIndex"
              }
            ]
          }
        },
        {
          "name": "spotMarketMint"
        },
        {
          "name": "constituentVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84,
                  95,
                  86,
                  65,
                  85,
                  76,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "spotMarketIndex"
              }
            ]
          }
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "spotMarketIndex",
          "type": "u16"
        },
        {
          "name": "decimals",
          "type": "u8"
        },
        {
          "name": "maxWeightDeviation",
          "type": "i64"
        },
        {
          "name": "swapFeeMin",
          "type": "i64"
        },
        {
          "name": "swapFeeMax",
          "type": "i64"
        },
        {
          "name": "maxBorrowTokenAmount",
          "type": "u64"
        },
        {
          "name": "oracleStalenessThreshold",
          "type": "u64"
        },
        {
          "name": "costToTrade",
          "type": "i32"
        },
        {
          "name": "constituentDerivativeIndex",
          "type": {
            "option": "i16"
          }
        },
        {
          "name": "constituentDerivativeDepegThreshold",
          "type": "u64"
        },
        {
          "name": "derivativeWeight",
          "type": "u64"
        },
        {
          "name": "volatility",
          "type": "u64"
        },
        {
          "name": "gammaExecution",
          "type": "u8"
        },
        {
          "name": "gammaInventory",
          "type": "u8"
        },
        {
          "name": "xi",
          "type": "u8"
        },
        {
          "name": "newConstituentCorrelations",
          "type": {
            "vec": "i64"
          }
        }
      ]
    },
    {
      "name": "initializeCrankTreasury",
      "docs": [
        "Create the program's shared resolver staging account (one for the",
        "whole program; permissionless, pays its own rent once).",
        "Create the protocol's relay crank treasury — the one account that funds",
        "every market's crank reservoir. Born unpriced; `update_crank_treasury`",
        "decides what it spends."
      ],
      "discriminator": [
        192,
        223,
        190,
        57,
        205,
        128,
        10,
        235
      ],
      "accounts": [
        {
          "name": "treasury",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  116,
                  114,
                  101,
                  97,
                  115,
                  117,
                  114,
                  121
                ]
              }
            ]
          }
        },
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": []
    },
    {
      "name": "initializeInsuranceFundStake",
      "discriminator": [
        187,
        179,
        243,
        70,
        248,
        90,
        92,
        147
      ],
      "accounts": [
        {
          "name": "spotMarket",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "insuranceFundStake",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  105,
                  110,
                  115,
                  117,
                  114,
                  97,
                  110,
                  99,
                  101,
                  95,
                  102,
                  117,
                  110,
                  100,
                  95,
                  115,
                  116,
                  97,
                  107,
                  101
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "userStats"
          ]
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "initializeLpPool",
      "discriminator": [
        242,
        64,
        1,
        222,
        142,
        46,
        204,
        227
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "lpPool",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  108,
                  112,
                  95,
                  112,
                  111,
                  111,
                  108
                ]
              },
              {
                "kind": "arg",
                "path": "id"
              }
            ]
          }
        },
        {
          "name": "mint"
        },
        {
          "name": "lpPoolTokenVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  76,
                  80,
                  95,
                  80,
                  79,
                  79,
                  76,
                  95,
                  84,
                  79,
                  75,
                  69,
                  78,
                  95,
                  86,
                  65,
                  85,
                  76,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "ammConstituentMapping",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  65,
                  77,
                  77,
                  95,
                  77,
                  65,
                  80
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "constituentTargetBase",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  111,
                  110,
                  115,
                  116,
                  105,
                  116,
                  117,
                  101,
                  110,
                  116,
                  95,
                  116,
                  97,
                  114,
                  103,
                  101,
                  116,
                  95,
                  98,
                  97,
                  115,
                  101,
                  95,
                  115,
                  101,
                  101,
                  100
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "constituentCorrelations",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  111,
                  110,
                  115,
                  116,
                  105,
                  116,
                  117,
                  101,
                  110,
                  116,
                  95,
                  99,
                  111,
                  114,
                  114,
                  101,
                  108,
                  97,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "state"
        },
        {
          "name": "tokenProgram",
          "address": "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA"
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "lpPoolId",
          "type": "u8"
        },
        {
          "name": "minMintFee",
          "type": "i64"
        },
        {
          "name": "maxAum",
          "type": "u128"
        },
        {
          "name": "maxSettleQuoteAmountPerMarket",
          "type": "u64"
        },
        {
          "name": "whitelistMint",
          "type": "pubkey"
        }
      ]
    },
    {
      "name": "initializePerpMarket",
      "discriminator": [
        132,
        9,
        229,
        118,
        117,
        118,
        117,
        62
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "perpMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  112,
                  101,
                  114,
                  112,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "state"
              }
            ]
          }
        },
        {
          "name": "oracle"
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "ammBaseAssetReserve",
          "type": "u128"
        },
        {
          "name": "ammQuoteAssetReserve",
          "type": "u128"
        },
        {
          "name": "ammPeriodicity",
          "type": "i64"
        },
        {
          "name": "ammPegMultiplier",
          "type": "u128"
        },
        {
          "name": "oracleSource",
          "type": {
            "defined": {
              "name": "oracleSource"
            }
          }
        },
        {
          "name": "contractTier",
          "type": {
            "defined": {
              "name": "contractTier"
            }
          }
        },
        {
          "name": "marginRatioInitial",
          "type": "u32"
        },
        {
          "name": "marginRatioMaintenance",
          "type": "u32"
        },
        {
          "name": "liquidatorFee",
          "type": "u32"
        },
        {
          "name": "ifLiquidationFee",
          "type": "u32"
        },
        {
          "name": "imfFactor",
          "type": "u32"
        },
        {
          "name": "activeStatus",
          "type": "bool"
        },
        {
          "name": "baseSpread",
          "type": "u32"
        },
        {
          "name": "maxSpread",
          "type": "u32"
        },
        {
          "name": "maxOpenInterest",
          "type": "u128"
        },
        {
          "name": "maxRevenueWithdrawPerPeriod",
          "type": "u64"
        },
        {
          "name": "quoteMaxInsurance",
          "type": "u64"
        },
        {
          "name": "orderStepSize",
          "type": "u64"
        },
        {
          "name": "orderTickSize",
          "type": "u64"
        },
        {
          "name": "minOrderSize",
          "type": "u64"
        },
        {
          "name": "concentrationCoefScale",
          "type": "u128"
        },
        {
          "name": "curveUpdateIntensity",
          "type": "u8"
        },
        {
          "name": "ammJitIntensity",
          "type": "u8"
        },
        {
          "name": "name",
          "type": {
            "array": [
              "u8",
              32
            ]
          }
        },
        {
          "name": "lpPoolId",
          "type": "u8"
        },
        {
          "name": "fundingClampThreshold",
          "type": "u32"
        },
        {
          "name": "fundingRampSlope",
          "type": "u32"
        }
      ]
    },
    {
      "name": "initializePrelaunchOracle",
      "discriminator": [
        169,
        178,
        84,
        25,
        175,
        62,
        29,
        247
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "prelaunchOracle",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  112,
                  114,
                  101,
                  108,
                  97,
                  117,
                  110,
                  99,
                  104,
                  95,
                  111,
                  114,
                  97,
                  99,
                  108,
                  101
                ]
              },
              {
                "kind": "arg",
                "path": "params.perp_market_index"
              }
            ]
          }
        },
        {
          "name": "state"
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "prelaunchOracleParams"
            }
          }
        }
      ]
    },
    {
      "name": "initializePythLazerOracle",
      "discriminator": [
        140,
        107,
        33,
        214,
        235,
        219,
        103,
        20
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "lazerOracle",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  112,
                  121,
                  116,
                  104,
                  95,
                  108,
                  97,
                  122,
                  101,
                  114
                ]
              },
              {
                "kind": "arg",
                "path": "feedId"
              }
            ]
          }
        },
        {
          "name": "state"
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "feedId",
          "type": "u32"
        }
      ]
    },
    {
      "name": "initializeQuoter",
      "discriminator": [
        95,
        22,
        79,
        28,
        163,
        15,
        117,
        109
      ],
      "accounts": [
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "authority",
          "docs": [
            "Becomes `QuoterV0::authority` — manages the entry's config."
          ],
          "signer": true
        },
        {
          "name": "quoter",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114
                ]
              },
              {
                "kind": "arg",
                "path": "args.market_index"
              },
              {
                "kind": "account",
                "path": "quoterProgram"
              },
              {
                "kind": "account",
                "path": "user"
              }
            ]
          }
        },
        {
          "name": "perpMarket",
          "docs": [
            "Written when the entry is the market's book: a Clob-type entry becomes",
            "the market's `clob_quoter` here, once and for good."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  112,
                  101,
                  114,
                  112,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "args.market_index"
              }
            ]
          }
        },
        {
          "name": "state",
          "docs": [
            "Read for the admin check a non-Custom type needs."
          ]
        },
        {
          "name": "quoterProgram"
        },
        {
          "name": "user",
          "docs": [
            "loads it and requires `authority` to be its authority (creation is",
            "consent). Ignored for Vamm/Clob-type entries."
          ]
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "args",
          "type": {
            "defined": {
              "name": "initializeQuoterArgs"
            }
          }
        }
      ]
    },
    {
      "name": "initializeQuoterCrossConditions",
      "docs": [
        "Stand up (or re-price) a Custom quoter's relay cross-discovery",
        "conditions — permissionless; rent on the caller."
      ],
      "discriminator": [
        22,
        71,
        18,
        82,
        184,
        158,
        94,
        93
      ],
      "accounts": [
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "quoter",
          "docs": [
            "The Custom entry to discover crosses for."
          ]
        },
        {
          "name": "perpMarket",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  112,
                  101,
                  114,
                  112,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "quoter"
              }
            ]
          }
        },
        {
          "name": "clobQuoter",
          "docs": [
            "The market's canonical CLOB entry — the other leg of every staged",
            "cross."
          ]
        },
        {
          "name": "marketConditions",
          "docs": [
            "The market's crank conditions: the keeper-payment source of truth."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "quoter"
              }
            ]
          }
        },
        {
          "name": "crossConditions",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  99,
                  114,
                  111,
                  115,
                  115,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "quoter"
              }
            ]
          }
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "expireFallbackSlots",
          "type": "u64"
        }
      ]
    },
    {
      "name": "initializeReferrerName",
      "discriminator": [
        235,
        126,
        231,
        10,
        42,
        164,
        26,
        61
      ],
      "accounts": [
        {
          "name": "referrerName",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  114,
                  101,
                  102,
                  101,
                  114,
                  114,
                  101,
                  114,
                  95,
                  110,
                  97,
                  109,
                  101
                ]
              },
              {
                "kind": "arg",
                "path": "name"
              }
            ]
          }
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "name",
          "type": {
            "array": [
              "u8",
              32
            ]
          }
        }
      ]
    },
    {
      "name": "initializeRelayScratch",
      "discriminator": [
        87,
        141,
        72,
        84,
        238,
        1,
        115,
        131
      ],
      "accounts": [
        {
          "name": "scratch",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  114,
                  101,
                  108,
                  97,
                  121,
                  95,
                  115,
                  99,
                  114,
                  97,
                  116,
                  99,
                  104
                ]
              }
            ]
          }
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": []
    },
    {
      "name": "initializeRevenueShare",
      "discriminator": [
        57,
        9,
        123,
        131,
        82,
        52,
        50,
        13
      ],
      "accounts": [
        {
          "name": "revenueShare",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  82,
                  69,
                  86,
                  95,
                  83,
                  72,
                  65,
                  82,
                  69
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        },
        {
          "name": "authority"
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": []
    },
    {
      "name": "initializeRevenueShareEscrow",
      "discriminator": [
        187,
        18,
        123,
        88,
        238,
        104,
        84,
        154
      ],
      "accounts": [
        {
          "name": "escrow",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  82,
                  69,
                  86,
                  95,
                  69,
                  83,
                  67,
                  82,
                  79,
                  87
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        },
        {
          "name": "authority",
          "relations": [
            "userStats"
          ]
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "state"
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "numOrders",
          "type": "u16"
        }
      ]
    },
    {
      "name": "initializeRouterQuoteBuffer",
      "discriminator": [
        19,
        61,
        93,
        219,
        121,
        124,
        63,
        187
      ],
      "accounts": [
        {
          "name": "quoteBuffer",
          "docs": [
            "Pre-created, zeroed, velocity-owned, and sized `RouterQuoteBufferV0::SIZE`."
          ],
          "writable": true
        },
        {
          "name": "authority",
          "docs": [
            "The only signer that may later quote into this buffer."
          ],
          "signer": true
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "initializeSignedMsgUserOrders",
      "discriminator": [
        164,
        99,
        156,
        126,
        156,
        57,
        99,
        180
      ],
      "accounts": [
        {
          "name": "signedMsgUserOrders",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  83,
                  73,
                  71,
                  78,
                  69,
                  68,
                  95,
                  77,
                  83,
                  71
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        },
        {
          "name": "authority"
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "numOrders",
          "type": "u16"
        }
      ]
    },
    {
      "name": "initializeSignedMsgWsDelegates",
      "discriminator": [
        40,
        132,
        96,
        219,
        184,
        193,
        80,
        8
      ],
      "accounts": [
        {
          "name": "signedMsgWsDelegates",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  83,
                  73,
                  71,
                  78,
                  69,
                  68,
                  95,
                  77,
                  83,
                  71,
                  95,
                  87,
                  83
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        },
        {
          "name": "authority",
          "writable": true,
          "signer": true
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "delegates",
          "type": {
            "vec": "pubkey"
          }
        }
      ]
    },
    {
      "name": "initializeSpotMarket",
      "discriminator": [
        234,
        196,
        128,
        44,
        94,
        15,
        48,
        201
      ],
      "accounts": [
        {
          "name": "spotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "state"
              }
            ]
          }
        },
        {
          "name": "spotMarketMint"
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "state"
              }
            ]
          }
        },
        {
          "name": "insuranceFundVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  105,
                  110,
                  115,
                  117,
                  114,
                  97,
                  110,
                  99,
                  101,
                  95,
                  102,
                  117,
                  110,
                  100,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "state"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "oracle"
        },
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "optimalUtilization",
          "type": "u32"
        },
        {
          "name": "optimalBorrowRate",
          "type": "u32"
        },
        {
          "name": "maxBorrowRate",
          "type": "u32"
        },
        {
          "name": "oracleSource",
          "type": {
            "defined": {
              "name": "oracleSource"
            }
          }
        },
        {
          "name": "initialAssetWeight",
          "type": "u32"
        },
        {
          "name": "maintenanceAssetWeight",
          "type": "u32"
        },
        {
          "name": "initialLiabilityWeight",
          "type": "u32"
        },
        {
          "name": "maintenanceLiabilityWeight",
          "type": "u32"
        },
        {
          "name": "imfFactor",
          "type": "u32"
        },
        {
          "name": "liquidatorFee",
          "type": "u32"
        },
        {
          "name": "ifLiquidationFee",
          "type": "u32"
        },
        {
          "name": "activeStatus",
          "type": "bool"
        },
        {
          "name": "assetTier",
          "type": {
            "defined": {
              "name": "assetTier"
            }
          }
        },
        {
          "name": "scaleInitialAssetWeightStart",
          "type": "u64"
        },
        {
          "name": "withdrawGuardThreshold",
          "type": "u64"
        },
        {
          "name": "orderTickSize",
          "type": "u64"
        },
        {
          "name": "orderStepSize",
          "type": "u64"
        },
        {
          "name": "ifTotalFactor",
          "type": "u32"
        },
        {
          "name": "name",
          "type": {
            "array": [
              "u8",
              32
            ]
          }
        }
      ]
    },
    {
      "name": "initializeUser",
      "discriminator": [
        111,
        17,
        185,
        250,
        60,
        122,
        38,
        254
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              },
              {
                "kind": "arg",
                "path": "subAccountId"
              }
            ]
          }
        },
        {
          "name": "userConditions",
          "docs": [
            "Relay liquidation coverage, created alongside the account it",
            "watches. Optional so raw-instruction integrators aren't broken and",
            "so a caller can decline the rent; the SDK passes it by default, and",
            "`deploy-scripts/migrate.ts` backfills whatever was declined. Coming",
            "up empty is fine — the first sync writes the thresholds."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "user"
              }
            ]
          }
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "authority",
          "relations": [
            "userStats"
          ]
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "subAccountId",
          "type": "u16"
        },
        {
          "name": "name",
          "type": {
            "array": [
              "u8",
              32
            ]
          }
        }
      ]
    },
    {
      "name": "initializeUserStats",
      "discriminator": [
        254,
        243,
        72,
        98,
        251,
        130,
        168,
        213
      ],
      "accounts": [
        {
          "name": "userStats",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114,
                  95,
                  115,
                  116,
                  97,
                  116,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        },
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "authority"
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": []
    },
    {
      "name": "liquidateBorrowForPerpPnl",
      "discriminator": [
        169,
        17,
        32,
        90,
        207,
        148,
        209,
        27
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "liquidator",
          "writable": true
        },
        {
          "name": "liquidatorStats",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "perpMarketIndex",
          "type": "u16"
        },
        {
          "name": "spotMarketIndex",
          "type": "u16"
        },
        {
          "name": "liquidatorMaxLiabilityTransfer",
          "type": "u128"
        },
        {
          "name": "limitPrice",
          "type": {
            "option": "u64"
          }
        }
      ]
    },
    {
      "name": "liquidatePerp",
      "discriminator": [
        75,
        35,
        119,
        247,
        191,
        18,
        139,
        2
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "docs": [
            "program-keeper mode (protocol `User` as liquidator, relay turners —",
            "`liquidate_perp_with_fill` ONLY, the plain path rejects it) it is",
            "only the lamport payout target and no signature is required."
          ],
          "writable": true
        },
        {
          "name": "liquidator",
          "writable": true
        },
        {
          "name": "liquidatorStats",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "crankConditions",
          "docs": [
            "The fired market's crank conditions — the reservoir that pays the",
            "keeper in program-keeper mode (validated against `market_index` in",
            "the handler). Required in program-keeper mode."
          ],
          "writable": true,
          "optional": true
        },
        {
          "name": "instructionsSysvar",
          "docs": [
            "crank that wants its priority fee reimbursed — the fee is stated in",
            "the transaction's own compute-budget instructions and read back from",
            "here. Absent, the crank takes the flat payment."
          ],
          "optional": true,
          "address": "Sysvar1nstructions1111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "liquidatorMaxBaseAssetAmount",
          "type": "u64"
        },
        {
          "name": "limitPrice",
          "type": {
            "option": "u64"
          }
        }
      ]
    },
    {
      "name": "liquidatePerpPnlForDeposit",
      "discriminator": [
        237,
        75,
        198,
        235,
        233,
        186,
        75,
        35
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "liquidator",
          "writable": true
        },
        {
          "name": "liquidatorStats",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "perpMarketIndex",
          "type": "u16"
        },
        {
          "name": "spotMarketIndex",
          "type": "u16"
        },
        {
          "name": "liquidatorMaxPnlTransfer",
          "type": "u128"
        },
        {
          "name": "limitPrice",
          "type": {
            "option": "u64"
          }
        }
      ]
    },
    {
      "name": "liquidatePerpWithFill",
      "discriminator": [
        95,
        111,
        124,
        105,
        86,
        169,
        187,
        34
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "docs": [
            "program-keeper mode (protocol `User` as liquidator, relay turners —",
            "`liquidate_perp_with_fill` ONLY, the plain path rejects it) it is",
            "only the lamport payout target and no signature is required."
          ],
          "writable": true
        },
        {
          "name": "liquidator",
          "writable": true
        },
        {
          "name": "liquidatorStats",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "crankConditions",
          "docs": [
            "The fired market's crank conditions — the reservoir that pays the",
            "keeper in program-keeper mode (validated against `market_index` in",
            "the handler). Required in program-keeper mode."
          ],
          "writable": true,
          "optional": true
        },
        {
          "name": "instructionsSysvar",
          "docs": [
            "crank that wants its priority fee reimbursed — the fee is stated in",
            "the transaction's own compute-budget instructions and read back from",
            "here. Absent, the crank takes the flat payment."
          ],
          "optional": true,
          "address": "Sysvar1nstructions1111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "liquidateSpot",
      "discriminator": [
        107,
        0,
        128,
        41,
        35,
        229,
        251,
        18
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "docs": [
            "A spot liquidation settles by handing the liquidator the borrow and",
            "the collateral behind it, so whoever liquidates takes on that inventory",
            "and its price risk. That rules out a protocol keeper, which has no way",
            "to unwind it, and therefore rules out relay: an executor may name no",
            "signer, so a path that requires one is a signed-keeper path only."
          ],
          "signer": true
        },
        {
          "name": "liquidator",
          "writable": true
        },
        {
          "name": "liquidatorStats"
        },
        {
          "name": "user",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "assetMarketIndex",
          "type": "u16"
        },
        {
          "name": "liabilityMarketIndex",
          "type": "u16"
        },
        {
          "name": "liquidatorMaxLiabilityTransfer",
          "type": "u128"
        },
        {
          "name": "limitPrice",
          "type": {
            "option": "u64"
          }
        }
      ]
    },
    {
      "name": "liquidateSpotWithSwapBegin",
      "discriminator": [
        12,
        43,
        176,
        83,
        156,
        251,
        117,
        13
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "liquidator",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "liabilitySpotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "liabilityMarketIndex"
              }
            ]
          }
        },
        {
          "name": "assetSpotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "assetMarketIndex"
              }
            ]
          }
        },
        {
          "name": "liabilityTokenAccount",
          "writable": true
        },
        {
          "name": "assetTokenAccount",
          "writable": true
        },
        {
          "name": "tokenProgram"
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "instructions",
          "docs": [
            "Instructions Sysvar for instruction introspection"
          ],
          "address": "Sysvar1nstructions1111111111111111111111111"
        },
        {
          "name": "liquidatorStats",
          "docs": [
            "The liquidator's `UserStats`, read by `begin` to bar an authority whose",
            "equity breaker is tripped.",
            "",
            "It sits last, not beside `liquidator` where the direct liquidation",
            "contexts carry it, because this pair is addressed by position rather",
            "than by name: `begin` introspects the matching `end` and compares the",
            "two account lists index by index, and the swap accounts both forward",
            "begin where this fixed block ends. Taking the last slot renumbered",
            "nothing. Slotting it beside `liquidator` would have moved `user`, both",
            "vaults and both token accounts down one, silently invalidating every",
            "hand-built transaction that still filled the old order."
          ]
        }
      ],
      "args": [
        {
          "name": "assetMarketIndex",
          "type": "u16"
        },
        {
          "name": "liabilityMarketIndex",
          "type": "u16"
        },
        {
          "name": "swapAmount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "liquidateSpotWithSwapEnd",
      "discriminator": [
        142,
        88,
        163,
        160,
        223,
        75,
        55,
        225
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "liquidator",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "liabilitySpotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "liabilityMarketIndex"
              }
            ]
          }
        },
        {
          "name": "assetSpotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "assetMarketIndex"
              }
            ]
          }
        },
        {
          "name": "liabilityTokenAccount",
          "writable": true
        },
        {
          "name": "assetTokenAccount",
          "writable": true
        },
        {
          "name": "tokenProgram"
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "instructions",
          "docs": [
            "Instructions Sysvar for instruction introspection"
          ],
          "address": "Sysvar1nstructions1111111111111111111111111"
        },
        {
          "name": "liquidatorStats",
          "docs": [
            "The liquidator's `UserStats`, read by `begin` to bar an authority whose",
            "equity breaker is tripped.",
            "",
            "It sits last, not beside `liquidator` where the direct liquidation",
            "contexts carry it, because this pair is addressed by position rather",
            "than by name: `begin` introspects the matching `end` and compares the",
            "two account lists index by index, and the swap accounts both forward",
            "begin where this fixed block ends. Taking the last slot renumbered",
            "nothing. Slotting it beside `liquidator` would have moved `user`, both",
            "vaults and both token accounts down one, silently invalidating every",
            "hand-built transaction that still filled the old order."
          ]
        }
      ],
      "args": [
        {
          "name": "assetMarketIndex",
          "type": "u16"
        },
        {
          "name": "liabilityMarketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "logUserBalances",
      "discriminator": [
        162,
        21,
        35,
        251,
        32,
        57,
        161,
        210
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "user",
          "writable": true
        }
      ],
      "args": []
    },
    {
      "name": "lpPoolAddLiquidity",
      "discriminator": [
        49,
        135,
        246,
        103,
        93,
        146,
        220,
        141
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "lpPool",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "inMarketMint"
        },
        {
          "name": "inConstituent",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "inMarketIndex"
              }
            ]
          }
        },
        {
          "name": "userInTokenAccount",
          "writable": true
        },
        {
          "name": "constituentInTokenAccount",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84,
                  95,
                  86,
                  65,
                  85,
                  76,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "inMarketIndex"
              }
            ]
          }
        },
        {
          "name": "userLpTokenAccount",
          "writable": true
        },
        {
          "name": "lpMint",
          "writable": true
        },
        {
          "name": "constituentTargetBase"
        },
        {
          "name": "lpPoolTokenVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  76,
                  80,
                  95,
                  80,
                  79,
                  79,
                  76,
                  95,
                  84,
                  79,
                  75,
                  69,
                  78,
                  95,
                  86,
                  65,
                  85,
                  76,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "inMarketIndex",
          "type": "u16"
        },
        {
          "name": "inAmount",
          "type": "u128"
        },
        {
          "name": "minMintAmount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "lpPoolRemoveLiquidity",
      "discriminator": [
        164,
        36,
        193,
        252,
        196,
        157,
        138,
        43
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "lpPool",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "outMarketMint"
        },
        {
          "name": "outConstituent",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "outMarketIndex"
              }
            ]
          }
        },
        {
          "name": "userOutTokenAccount",
          "writable": true
        },
        {
          "name": "constituentOutTokenAccount",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84,
                  95,
                  86,
                  65,
                  85,
                  76,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "outMarketIndex"
              }
            ]
          }
        },
        {
          "name": "userLpTokenAccount",
          "writable": true
        },
        {
          "name": "spotMarketTokenAccount",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "outMarketIndex"
              }
            ]
          }
        },
        {
          "name": "lpMint",
          "writable": true
        },
        {
          "name": "constituentTargetBase"
        },
        {
          "name": "lpPoolTokenVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  76,
                  80,
                  95,
                  80,
                  79,
                  79,
                  76,
                  95,
                  84,
                  79,
                  75,
                  69,
                  78,
                  95,
                  86,
                  65,
                  85,
                  76,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "tokenProgram"
        },
        {
          "name": "ammCache",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  97,
                  109,
                  109,
                  95,
                  99,
                  97,
                  99,
                  104,
                  101,
                  95,
                  115,
                  101,
                  101,
                  100
                ]
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "inMarketIndex",
          "type": "u16"
        },
        {
          "name": "inAmount",
          "type": "u64"
        },
        {
          "name": "minOutAmount",
          "type": "u128"
        }
      ]
    },
    {
      "name": "lpPoolSwap",
      "discriminator": [
        36,
        161,
        39,
        49,
        227,
        1,
        35,
        226
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "lpPool"
        },
        {
          "name": "constituentTargetBase"
        },
        {
          "name": "constituentCorrelations"
        },
        {
          "name": "constituentInTokenAccount",
          "writable": true
        },
        {
          "name": "constituentOutTokenAccount",
          "writable": true
        },
        {
          "name": "userInTokenAccount",
          "writable": true
        },
        {
          "name": "userOutTokenAccount",
          "writable": true
        },
        {
          "name": "inConstituent",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "inMarketIndex"
              }
            ]
          }
        },
        {
          "name": "outConstituent",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "outMarketIndex"
              }
            ]
          }
        },
        {
          "name": "inMarketMint"
        },
        {
          "name": "outMarketMint"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "inMarketIndex",
          "type": "u16"
        },
        {
          "name": "outMarketIndex",
          "type": "u16"
        },
        {
          "name": "inAmount",
          "type": "u64"
        },
        {
          "name": "minOutAmount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "modifyClobOrder",
      "docs": [
        "Reprice/resize a resting CLOB order: cancel-and-replace in one",
        "instruction, with a single margin gate over the net change. `None`",
        "fields keep the resting order's value."
      ],
      "discriminator": [
        64,
        222,
        242,
        64,
        138,
        91,
        220,
        67
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "quoter",
          "docs": [
            "The book's registry entry. The replacement leg additionally requires it",
            "to be active and approved."
          ]
        },
        {
          "name": "clobMarket",
          "docs": [
            "accounts in the handler."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "set to. Deliberately not the vault authority: signer privilege is",
            "inherited by a callee, so the key velocity hands an external program",
            "must be the authority on nothing."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        },
        {
          "name": "instructionsSysvar",
          "docs": [
            "faster-than-default activation delay on the replacement."
          ],
          "optional": true,
          "address": "Sysvar1nstructions1111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "modifyClobOrderParams"
            }
          }
        }
      ]
    },
    {
      "name": "modifyOrder",
      "discriminator": [
        47,
        124,
        117,
        255,
        201,
        197,
        130,
        94
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "orderId",
          "type": {
            "option": "u32"
          }
        },
        {
          "name": "modifyOrderParams",
          "type": {
            "defined": {
              "name": "modifyOrderParams"
            }
          }
        }
      ]
    },
    {
      "name": "modifyOrderByUserId",
      "discriminator": [
        158,
        77,
        4,
        253,
        252,
        194,
        161,
        179
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "userOrderId",
          "type": "u8"
        },
        {
          "name": "modifyOrderParams",
          "type": {
            "defined": {
              "name": "modifyOrderParams"
            }
          }
        }
      ]
    },
    {
      "name": "moveAmmPrice",
      "discriminator": [
        235,
        109,
        2,
        82,
        219,
        118,
        6,
        159
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "baseAssetReserve",
          "type": "u128"
        },
        {
          "name": "quoteAssetReserve",
          "type": "u128"
        },
        {
          "name": "sqrtK",
          "type": "u128"
        }
      ]
    },
    {
      "name": "overrideAmmCacheInfo",
      "discriminator": [
        189,
        198,
        128,
        9,
        49,
        145,
        201,
        115
      ],
      "accounts": [
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "ammCache",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  97,
                  109,
                  109,
                  95,
                  99,
                  97,
                  99,
                  104,
                  101,
                  95,
                  115,
                  101,
                  101,
                  100
                ]
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "overrideParams",
          "type": {
            "defined": {
              "name": "overrideAmmCacheParams"
            }
          }
        }
      ]
    },
    {
      "name": "pauseSpotMarketDepositWithdraw",
      "discriminator": [
        183,
        119,
        59,
        170,
        137,
        35,
        242,
        86
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "keeper",
          "signer": true
        },
        {
          "name": "spotMarket",
          "writable": true
        },
        {
          "name": "spotMarketVault",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "spotMarket"
              }
            ]
          }
        }
      ],
      "args": []
    },
    {
      "name": "placeAndMakePerpOrder",
      "discriminator": [
        149,
        117,
        11,
        237,
        47,
        95,
        89,
        237
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "taker",
          "writable": true
        },
        {
          "name": "takerStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "orderParams"
            }
          }
        },
        {
          "name": "takerOrderId",
          "type": "u32"
        }
      ]
    },
    {
      "name": "placeAndMakePerpOrderV1",
      "docs": [
        "`place_and_make_perp_order` with the market's CLOB accounts required:",
        "the unmatched remainder rests on the book instead of being cancelled.",
        "v0's account list is frozen for ABI compatibility, so the CLOB route is",
        "a separate endpoint rather than optional accounts on v0."
      ],
      "discriminator": [
        29,
        136,
        72,
        149,
        63,
        222,
        134,
        96
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "taker",
          "writable": true
        },
        {
          "name": "takerStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "quoter",
          "docs": [
            "The market's CLOB registry entry — the remainder only ever rests on a",
            "vetted book."
          ]
        },
        {
          "name": "clobMarket",
          "docs": [
            "(`ClobMarket::from_quoter`), so a valid entry can't be pointed at an",
            "arbitrary account."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "set to, and the authority on nothing else."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "Wake-hint host for the rested remainder. Optional like every other CLOB",
            "placement path: a market whose conditions were never initialized must",
            "still be tradeable, and a missed hint costs crank latency, not liveness."
          ],
          "writable": true,
          "optional": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "arg",
                "path": "params.market_index"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "orderParams"
            }
          }
        },
        {
          "name": "takerOrderId",
          "type": "u32"
        }
      ]
    },
    {
      "name": "placeAndMakeSignedMsgPerpOrder",
      "discriminator": [
        16,
        26,
        123,
        131,
        94,
        29,
        175,
        98
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "taker",
          "writable": true
        },
        {
          "name": "takerStats",
          "writable": true
        },
        {
          "name": "takerSignedMsgUserOrders",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  83,
                  73,
                  71,
                  78,
                  69,
                  68,
                  95,
                  77,
                  83,
                  71
                ]
              },
              {
                "kind": "account",
                "path": "taker"
              }
            ]
          }
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "orderParams"
            }
          }
        },
        {
          "name": "signedMsgOrderUuid",
          "type": {
            "array": [
              "u8",
              8
            ]
          }
        }
      ]
    },
    {
      "name": "placeAndTakePerpOrder",
      "discriminator": [
        213,
        51,
        1,
        187,
        108,
        220,
        230,
        224
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "orderParams"
            }
          }
        },
        {
          "name": "successCondition",
          "type": {
            "option": "u32"
          }
        }
      ]
    },
    {
      "name": "placeAndTakePerpOrderV1",
      "docs": [
        "`place_and_take_perp_order` with the market's CLOB accounts required:",
        "an unfilled restable limit remainder rests on the book instead of the",
        "DLOB. v0's account list is frozen for ABI compatibility, so the CLOB",
        "route is a separate endpoint rather than optional accounts on v0."
      ],
      "discriminator": [
        168,
        73,
        91,
        161,
        18,
        41,
        252,
        94
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "quoter",
          "docs": [
            "The market's CLOB registry entry — the remainder only ever rests on a",
            "vetted book."
          ]
        },
        {
          "name": "clobMarket",
          "docs": [
            "accounts (`ClobMarket::from_quoter`), so a valid entry can't be",
            "pointed at an arbitrary account."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "set to. Deliberately not the vault authority: signer privilege is",
            "inherited by a callee, so the key velocity hands an external program",
            "must be the authority on nothing."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "Wake-hint host for the rested remainder. Optional like every other",
            "CLOB placement path: a market whose conditions were never initialized",
            "must still be tradeable, and a missed hint costs crank latency, not",
            "liveness (the fallback poll is the floor)."
          ],
          "writable": true,
          "optional": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "arg",
                "path": "params.market_index"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "orderParams"
            }
          }
        },
        {
          "name": "successCondition",
          "type": {
            "option": "u32"
          }
        }
      ]
    },
    {
      "name": "placeClobOrder",
      "discriminator": [
        252,
        250,
        165,
        51,
        142,
        80,
        101,
        210
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "quoter",
          "docs": [
            "The CLOB's registry entry for this market — placement is only allowed",
            "on a vetted book."
          ]
        },
        {
          "name": "clobMarket",
          "docs": [
            "accounts in the handler (the vetted CPI surface names the book)."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "set to. Deliberately not the vault authority: signer privilege is",
            "inherited by a callee, so the key velocity hands an external program",
            "must be the authority on nothing."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        },
        {
          "name": "instructionsSysvar",
          "docs": [
            "a faster-than-default activation delay: the handler introspects it",
            "for the flow-authority co-signer (the attestation)."
          ],
          "optional": true,
          "address": "Sysvar1nstructions1111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "placeClobOrderParams"
            }
          }
        }
      ]
    },
    {
      "name": "placeOrders",
      "discriminator": [
        60,
        63,
        50,
        123,
        12,
        197,
        60,
        190
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "vec": {
              "defined": {
                "name": "orderParams"
              }
            }
          }
        }
      ]
    },
    {
      "name": "placePerpOrder",
      "discriminator": [
        69,
        161,
        93,
        202,
        120,
        126,
        76,
        185
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "orderParams"
            }
          }
        }
      ]
    },
    {
      "name": "placeScaleOrders",
      "discriminator": [
        129,
        249,
        70,
        55,
        177,
        250,
        252,
        94
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "scaleOrderParams"
            }
          }
        }
      ]
    },
    {
      "name": "placeSignedMsgTakerOrder",
      "discriminator": [
        32,
        79,
        101,
        139,
        25,
        6,
        98,
        15
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "signedMsgUserOrders",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  83,
                  73,
                  71,
                  78,
                  69,
                  68,
                  95,
                  77,
                  83,
                  71
                ]
              },
              {
                "kind": "account",
                "path": "user"
              }
            ]
          }
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "ixSysvar",
          "docs": [
            "the supplied Sysvar could be anything else.",
            "The Instruction Sysvar has not been implemented",
            "in the Anchor framework yet, so this is the safe approach."
          ],
          "address": "Sysvar1nstructions1111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "signedMsgOrderParamsMessageBytes",
          "type": "bytes"
        },
        {
          "name": "isDelegateSigner",
          "type": "bool"
        }
      ]
    },
    {
      "name": "postPythLazerOracleUpdate",
      "discriminator": [
        218,
        237,
        170,
        245,
        39,
        143,
        166,
        33
      ],
      "accounts": [
        {
          "name": "keeper",
          "writable": true,
          "signer": true
        },
        {
          "name": "pythLazerStorage",
          "address": "3rdJbqfnagQ4yx9HXJViD4zc4xpiSqmFsKpPuSCQVyQL"
        },
        {
          "name": "ixSysvar",
          "address": "Sysvar1nstructions1111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "pythMessage",
          "type": "bytes"
        }
      ]
    },
    {
      "name": "quoteRouter",
      "docs": [
        "Read-only router quote: writes per-source verified books into the",
        "caller's quote buffer. Meant to be simulated, not landed."
      ],
      "discriminator": [
        130,
        18,
        102,
        250,
        85,
        231,
        54,
        71
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "quoteBuffer"
          ]
        },
        {
          "name": "quoteBuffer",
          "docs": [
            "`has_one` pins the writer; the market is checked in the handler",
            "because it comes in as an argument, not an account."
          ],
          "writable": true
        }
      ],
      "args": [
        {
          "name": "args",
          "type": {
            "defined": {
              "name": "quoteRouterArgs"
            }
          }
        }
      ]
    },
    {
      "name": "recenterPerpMarketAmm",
      "discriminator": [
        24,
        87,
        10,
        115,
        165,
        190,
        80,
        139
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "pegMultiplier",
          "type": "u128"
        },
        {
          "name": "sqrtK",
          "type": "u128"
        }
      ]
    },
    {
      "name": "recenterPerpMarketAmmCrank",
      "discriminator": [
        166,
        19,
        64,
        10,
        14,
        51,
        101,
        122
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "spotMarket",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "perpMarket"
              }
            ]
          }
        },
        {
          "name": "oracle"
        }
      ],
      "args": [
        {
          "name": "depth",
          "type": {
            "option": "u128"
          }
        }
      ]
    },
    {
      "name": "reclaimRent",
      "discriminator": [
        218,
        200,
        19,
        197,
        227,
        89,
        192,
        22
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "user",
            "userStats"
          ]
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        }
      ],
      "args": []
    },
    {
      "name": "refillCrankReservoir",
      "docs": [
        "Top a market's crank reservoir back up out of the protocol treasury —",
        "permissionless, and relay-cranked like the work it funds. Reverts",
        "while the reservoir is above its watermark, so it cannot be repeated",
        "for the payment."
      ],
      "discriminator": [
        65,
        90,
        188,
        226,
        191,
        95,
        208,
        169
      ],
      "accounts": [
        {
          "name": "treasury",
          "docs": [
            "The protocol's lamport pool."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  116,
                  114,
                  101,
                  97,
                  115,
                  117,
                  114,
                  121
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "The market reservoir being filled."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "authority",
          "docs": [
            "never signs, so a turner can name a payout account that is not the key",
            "paying for the transaction."
          ],
          "writable": true
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "refreshSpotMarketInterest",
      "discriminator": [
        11,
        188,
        50,
        141,
        73,
        51,
        134,
        78
      ],
      "accounts": [
        {
          "name": "state"
        }
      ],
      "args": [
        {
          "name": "marketIndexes",
          "type": {
            "vec": "u16"
          }
        }
      ]
    },
    {
      "name": "removeAmmConstituentMappingData",
      "discriminator": [
        20,
        183,
        211,
        162,
        16,
        52,
        229,
        115
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "lpPool"
        },
        {
          "name": "ammConstituentMapping",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  65,
                  77,
                  77,
                  95,
                  77,
                  65,
                  80
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        },
        {
          "name": "state"
        }
      ],
      "args": [
        {
          "name": "perpMarketIndex",
          "type": "u16"
        },
        {
          "name": "constituentIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "removeInsuranceFundStake",
      "discriminator": [
        128,
        166,
        142,
        9,
        254,
        187,
        143,
        174
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "insuranceFundStake",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "insuranceFundStake",
            "userStats"
          ]
        },
        {
          "name": "insuranceFundVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  105,
                  110,
                  115,
                  117,
                  114,
                  97,
                  110,
                  99,
                  101,
                  95,
                  102,
                  117,
                  110,
                  100,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "userTokenAccount",
          "writable": true
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "repegAmmCurve",
      "discriminator": [
        3,
        36,
        102,
        89,
        180,
        128,
        120,
        213
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "oracle"
        },
        {
          "name": "admin",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "newPegCandidate",
          "type": "u128"
        }
      ]
    },
    {
      "name": "requestRemoveInsuranceFundStake",
      "discriminator": [
        142,
        70,
        204,
        92,
        73,
        106,
        180,
        52
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "insuranceFundStake",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "insuranceFundStake",
            "userStats"
          ]
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "insuranceFundVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  105,
                  110,
                  115,
                  117,
                  114,
                  97,
                  110,
                  99,
                  101,
                  95,
                  102,
                  117,
                  110,
                  100,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "resetEquityFloorBreaker",
      "discriminator": [
        230,
        181,
        202,
        36,
        127,
        56,
        56,
        27
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "userStats",
          "writable": true
        }
      ],
      "args": []
    },
    {
      "name": "resetPerpMarketAmmOracleTwap",
      "discriminator": [
        127,
        10,
        55,
        164,
        123,
        226,
        47,
        24
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "oracle"
        },
        {
          "name": "admin",
          "signer": true
        }
      ],
      "args": []
    },
    {
      "name": "resizeRevenueShareEscrowOrders",
      "discriminator": [
        32,
        124,
        247,
        225,
        151,
        213,
        225,
        38
      ],
      "accounts": [
        {
          "name": "escrow",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  82,
                  69,
                  86,
                  95,
                  69,
                  83,
                  67,
                  82,
                  79,
                  87
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        },
        {
          "name": "authority",
          "relations": [
            "escrow"
          ]
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "numOrders",
          "type": "u16"
        }
      ]
    },
    {
      "name": "resizeSignedMsgUserOrders",
      "discriminator": [
        137,
        10,
        87,
        150,
        18,
        115,
        79,
        168
      ],
      "accounts": [
        {
          "name": "signedMsgUserOrders",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  83,
                  73,
                  71,
                  78,
                  69,
                  68,
                  95,
                  77,
                  83,
                  71
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        },
        {
          "name": "authority"
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "numOrders",
          "type": "u16"
        }
      ]
    },
    {
      "name": "resolveClobCrank",
      "docs": [
        "Relay resolver for the evict condition. Meant to be simulated, not",
        "landed: stages the executor call and returns a response pointer.",
        "Resolver for every condition a market's CLOB cranks wake on: an",
        "expired order, a side at its eviction threshold, the book crossing",
        "itself, and the poll that catches a cross a PropAMM created. Relay",
        "hands over which condition fired, so one resolver answers for all of",
        "them and stages the executor that fits."
      ],
      "discriminator": [
        0,
        10,
        93,
        76,
        45,
        249,
        99,
        54
      ],
      "accounts": [
        {
          "name": "scratch",
          "docs": [
            "The shared staging account, index 0 by convention — a resolver's",
            "response pointer is interpreted against it."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  114,
                  101,
                  108,
                  97,
                  121,
                  95,
                  115,
                  99,
                  114,
                  97,
                  116,
                  99,
                  104
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "Read-only: resolvers stage into the shared scratch account, not",
            "into the block they read."
          ]
        },
        {
          "name": "clobMarket",
          "docs": [
            "accounts, same as the executor it stages.",
            "",
            "Writable for the book's response tail: the cross resolver asks the book",
            "for its resting orders through `quote_l3_v0`, which streams the answer",
            "into that tail. Nothing a resolver sends ever lands, and the tail is a",
            "scratch region the book rewrites on every quote."
          ],
          "writable": true
        },
        {
          "name": "quoter"
        },
        {
          "name": "state"
        },
        {
          "name": "clobProgram",
          "docs": [
            "the book which order to remove instead of reading its arena, so it",
            "calls the program rather than parsing the account."
          ]
        },
        {
          "name": "treasury",
          "docs": [
            "Read-only: the refill resolver reads the levels a reservoir is held",
            "between, which are the treasury's setting rather than the market's."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  116,
                  114,
                  101,
                  97,
                  115,
                  117,
                  114,
                  121
                ]
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "fired",
          "type": {
            "defined": {
              "name": "firedConditionArgV0"
            }
          }
        }
      ]
    },
    {
      "name": "resolveCrankCrossMatchQuoter",
      "docs": [
        "Relay resolver for a Custom quoter's cross conditions: prices the",
        "quoter generically through its registered `quote_v0` surface and",
        "stages `crank_cross_match`. Meant to be simulated, not landed."
      ],
      "discriminator": [
        170,
        98,
        124,
        98,
        157,
        243,
        148,
        131
      ],
      "accounts": [
        {
          "name": "scratch",
          "docs": [
            "The shared staging account, index 0 by convention — a resolver's",
            "response pointer is interpreted against it."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  114,
                  101,
                  108,
                  97,
                  121,
                  95,
                  115,
                  99,
                  114,
                  97,
                  116,
                  99,
                  104
                ]
              }
            ]
          }
        },
        {
          "name": "crossConditions",
          "docs": [
            "Writable only for the staging region; simulation-only.",
            "Read-only: resolvers stage into the shared scratch account."
          ]
        },
        {
          "name": "clobMarket",
          "docs": [
            "book's response tail, which is where `quote_l3_v0` streams the resting",
            "orders this resolver crosses the entry against."
          ],
          "writable": true
        },
        {
          "name": "state"
        },
        {
          "name": "quoter",
          "relations": [
            "crossConditions"
          ]
        },
        {
          "name": "user",
          "docs": [
            "The entry's quoted user — the maker every staged balance change",
            "lands on; its identity derives the staged `(User, UserStats)` pair."
          ]
        },
        {
          "name": "clobQuoter",
          "docs": [
            "The market's CLOB registry entry: the other leg is quoted through the",
            "same registered interface as this one, so neither side is read out of",
            "an account."
          ]
        },
        {
          "name": "clobProgram"
        }
      ],
      "args": []
    },
    {
      "name": "resolveLiquidatePerpWithFill",
      "docs": [
        "Relay resolver for a liquidation threshold: runs the real margin",
        "calculation and stages `liquidate_perp_with_fill` with the protocol",
        "`User` as the (inventory-free) liquidator. Simulated, not landed."
      ],
      "discriminator": [
        170,
        221,
        26,
        188,
        165,
        176,
        102,
        89
      ],
      "accounts": [
        {
          "name": "scratch",
          "docs": [
            "The shared staging account, index 0 by convention — a resolver's",
            "response pointer is interpreted against it."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  114,
                  101,
                  108,
                  97,
                  121,
                  95,
                  115,
                  99,
                  114,
                  97,
                  116,
                  99,
                  104
                ]
              }
            ]
          }
        },
        {
          "name": "liqConditions",
          "docs": [
            "Read-only: resolvers stage into the shared scratch account, not",
            "into the block they read."
          ]
        },
        {
          "name": "user"
        },
        {
          "name": "state"
        }
      ],
      "args": []
    },
    {
      "name": "resolvePerpBankruptcy",
      "discriminator": [
        224,
        16,
        176,
        214,
        162,
        213,
        183,
        222
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "liquidator",
          "writable": true
        },
        {
          "name": "liquidatorStats",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "spotMarketIndex"
              }
            ]
          }
        },
        {
          "name": "insuranceFundVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  105,
                  110,
                  115,
                  117,
                  114,
                  97,
                  110,
                  99,
                  101,
                  95,
                  102,
                  117,
                  110,
                  100,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "spotMarketIndex"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "quoteSpotMarketIndex",
          "type": "u16"
        },
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "resolvePerpPnlDeficit",
      "discriminator": [
        168,
        204,
        68,
        150,
        159,
        126,
        95,
        148
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "spotMarketIndex"
              }
            ]
          }
        },
        {
          "name": "insuranceFundVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  105,
                  110,
                  115,
                  117,
                  114,
                  97,
                  110,
                  99,
                  101,
                  95,
                  102,
                  117,
                  110,
                  100,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "spotMarketIndex"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "spotMarketIndex",
          "type": "u16"
        },
        {
          "name": "perpMarketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "resolveResyncLiqConditions",
      "docs": [
        "Relay resolver for the self-sync conditions. Simulated, not landed."
      ],
      "discriminator": [
        192,
        11,
        237,
        99,
        246,
        44,
        57,
        175
      ],
      "accounts": [
        {
          "name": "scratch",
          "docs": [
            "The shared staging account, index 0 by convention — a resolver's",
            "response pointer is interpreted against it."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  114,
                  101,
                  108,
                  97,
                  121,
                  95,
                  115,
                  99,
                  114,
                  97,
                  116,
                  99,
                  104
                ]
              }
            ]
          }
        },
        {
          "name": "liqConditions",
          "docs": [
            "Read-only: resolvers stage into the shared scratch account, not",
            "into the block they read."
          ]
        },
        {
          "name": "user"
        }
      ],
      "args": []
    },
    {
      "name": "resolveSpotBankruptcy",
      "discriminator": [
        124,
        194,
        240,
        254,
        198,
        213,
        52,
        122
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "liquidator",
          "writable": true
        },
        {
          "name": "liquidatorStats",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "spotMarketIndex"
              }
            ]
          }
        },
        {
          "name": "insuranceFundVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  105,
                  110,
                  115,
                  117,
                  114,
                  97,
                  110,
                  99,
                  101,
                  95,
                  102,
                  117,
                  110,
                  100,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "spotMarketIndex"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "resolveTriggerClobOrder",
      "docs": [
        "Relay resolver for `trigger_clob_order`. Meant to be simulated, not",
        "landed."
      ],
      "discriminator": [
        21,
        250,
        194,
        125,
        230,
        11,
        202,
        66
      ],
      "accounts": [
        {
          "name": "scratch",
          "docs": [
            "The shared staging account, index 0 by convention — a resolver's",
            "response pointer is interpreted against it."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  114,
                  101,
                  108,
                  97,
                  121,
                  95,
                  115,
                  99,
                  114,
                  97,
                  116,
                  99,
                  104
                ]
              }
            ]
          }
        },
        {
          "name": "triggerConditions",
          "docs": [
            "Read-only: resolvers stage into the shared scratch account, not",
            "into the block they read."
          ]
        },
        {
          "name": "user"
        },
        {
          "name": "oracle"
        },
        {
          "name": "perpMarket"
        }
      ],
      "args": []
    },
    {
      "name": "resolveTriggerOrder",
      "docs": [
        "Relay resolver for `trigger_order`. Meant to be simulated, not landed."
      ],
      "discriminator": [
        246,
        112,
        254,
        98,
        43,
        234,
        178,
        33
      ],
      "accounts": [
        {
          "name": "scratch",
          "docs": [
            "The shared staging account, index 0 by convention — a resolver's",
            "response pointer is interpreted against it."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  114,
                  101,
                  108,
                  97,
                  121,
                  95,
                  115,
                  99,
                  114,
                  97,
                  116,
                  99,
                  104
                ]
              }
            ]
          }
        },
        {
          "name": "triggerConditions",
          "docs": [
            "Read-only: resolvers stage into the shared scratch account, not",
            "into the block they read."
          ]
        },
        {
          "name": "user"
        },
        {
          "name": "oracle"
        },
        {
          "name": "perpMarket"
        }
      ],
      "args": []
    },
    {
      "name": "resyncLiqConditions",
      "docs": [
        "Relay's unsigned self-maintenance path: rewrite an existing block",
        "and pay the keeper from its own lamports. Names no signer — staged",
        "executors are submitted unsigned."
      ],
      "discriminator": [
        1,
        22,
        75,
        53,
        225,
        51,
        245,
        190
      ],
      "accounts": [
        {
          "name": "keeper",
          "docs": [
            "slot. Never a signer (see the module doc); it only receives",
            "lamports."
          ],
          "writable": true
        },
        {
          "name": "user"
        },
        {
          "name": "liqConditions",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "user"
              }
            ]
          }
        },
        {
          "name": "treasury",
          "docs": [
            "The protocol pool this resync is paid from."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  116,
                  114,
                  101,
                  97,
                  115,
                  117,
                  114,
                  121
                ]
              }
            ]
          }
        }
      ],
      "args": []
    },
    {
      "name": "revertFill",
      "discriminator": [
        236,
        238,
        176,
        69,
        239,
        10,
        181,
        193
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "filler",
          "writable": true
        },
        {
          "name": "fillerStats",
          "writable": true
        }
      ],
      "args": []
    },
    {
      "name": "setUserStatusToBeingLiquidated",
      "discriminator": [
        106,
        133,
        160,
        206,
        193,
        171,
        192,
        194
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": []
    },
    {
      "name": "settleExpiredMarket",
      "discriminator": [
        120,
        89,
        11,
        25,
        122,
        77,
        72,
        193
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "settleExpiredMarketPoolsToRevenuePool",
      "discriminator": [
        55,
        19,
        238,
        169,
        227,
        90,
        200,
        184
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "spotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": []
    },
    {
      "name": "settleFundingPayment",
      "discriminator": [
        222,
        90,
        202,
        94,
        28,
        45,
        115,
        183
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        }
      ],
      "args": []
    },
    {
      "name": "settleMultiplePnls",
      "discriminator": [
        127,
        66,
        117,
        57,
        40,
        50,
        152,
        127
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "spotMarketVault",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "marketIndexes",
          "type": {
            "vec": "u16"
          }
        },
        {
          "name": "mode",
          "type": {
            "defined": {
              "name": "settlePnlMode"
            }
          }
        }
      ]
    },
    {
      "name": "settlePerpToLpPool",
      "discriminator": [
        5,
        98,
        46,
        188,
        10,
        59,
        2,
        249
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "lpPool",
          "writable": true
        },
        {
          "name": "keeper",
          "writable": true,
          "signer": true
        },
        {
          "name": "ammCache",
          "writable": true
        },
        {
          "name": "quoteMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        },
        {
          "name": "constituent",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        },
        {
          "name": "constituentQuoteTokenAccount",
          "writable": true
        },
        {
          "name": "quoteTokenVault",
          "writable": true
        },
        {
          "name": "tokenProgram"
        },
        {
          "name": "velocitySigner"
        }
      ],
      "args": []
    },
    {
      "name": "settlePnl",
      "discriminator": [
        43,
        61,
        234,
        45,
        15,
        95,
        152,
        153
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "spotMarketVault",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "settleRevenueShare",
      "discriminator": [
        21,
        123,
        155,
        221,
        194,
        240,
        233,
        76
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "escrowAuthority",
          "docs": [
            "The owner of the escrow to settle."
          ]
        },
        {
          "name": "revenueShareEscrow",
          "docs": [
            "The escrow that holds the accrued builder and referrer rows."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  82,
                  69,
                  86,
                  95,
                  69,
                  83,
                  67,
                  82,
                  79,
                  87
                ]
              },
              {
                "kind": "account",
                "path": "escrowAuthority"
              }
            ]
          }
        },
        {
          "name": "spotMarketVault",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "numOwnerSubAccounts",
          "type": "u8"
        }
      ]
    },
    {
      "name": "settleRevenueToInsuranceFund",
      "discriminator": [
        200,
        120,
        93,
        136,
        69,
        38,
        199,
        159
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "insuranceFundVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  105,
                  110,
                  115,
                  117,
                  114,
                  97,
                  110,
                  99,
                  101,
                  95,
                  102,
                  117,
                  110,
                  100,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "spotMarketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "specialTransferPerpPositionToVamm",
      "discriminator": [
        39,
        111,
        187,
        243,
        18,
        139,
        223,
        1
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "state"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": {
            "option": "i64"
          }
        }
      ]
    },
    {
      "name": "sweepCrankReservoir",
      "docs": [
        "Move lamports from a market's crank reservoir back to the treasury, so",
        "an over-provisioned or retired market does not hold them for good."
      ],
      "discriminator": [
        163,
        83,
        223,
        15,
        134,
        144,
        208,
        53
      ],
      "accounts": [
        {
          "name": "treasury",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  116,
                  114,
                  101,
                  97,
                  115,
                  117,
                  114,
                  121
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "lamports",
          "type": "u64"
        }
      ]
    },
    {
      "name": "sweepPerpMarketFees",
      "discriminator": [
        194,
        147,
        181,
        230,
        193,
        155,
        241,
        225
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  112,
                  101,
                  114,
                  112,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "perpMarketIndex"
              }
            ]
          }
        },
        {
          "name": "spotMarket",
          "docs": [
            "The perp market's quote spot market (enforced by the PDA derivation)"
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "perpMarket"
              }
            ]
          }
        },
        {
          "name": "oracle",
          "relations": [
            "perpMarket"
          ]
        }
      ],
      "args": [
        {
          "name": "perpMarketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "syncLiqConditions",
      "docs": [
        "Rewrite only the liquidation half of a user's condition block.",
        "Prefer `sync_user_conditions` unless the trigger half is known",
        "current. Staged by the block's own self-sync watch on position",
        "changes, which is why this half has a relay path and the other",
        "does not."
      ],
      "discriminator": [
        87,
        58,
        107,
        39,
        182,
        230,
        153,
        200
      ],
      "accounts": [
        {
          "name": "payer",
          "docs": [
            "sync it is the keeper payout target (paid from the conditions",
            "account's own lamports). Writable for both roles."
          ],
          "writable": true,
          "signer": true
        },
        {
          "name": "state",
          "docs": [
            "Read for the fee rails the sync's own keeper payment is priced from."
          ]
        },
        {
          "name": "user"
        },
        {
          "name": "liqConditions",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "user"
              }
            ]
          }
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "args",
          "type": {
            "defined": {
              "name": "syncLiqConditionsArgs"
            }
          }
        }
      ]
    },
    {
      "name": "syncTriggerConditions",
      "docs": [
        "Rewrite only the trigger half of a user's condition block. Prefer",
        "`sync_user_conditions` unless the liquidation half is known current."
      ],
      "discriminator": [
        105,
        93,
        218,
        179,
        236,
        229,
        95,
        132
      ],
      "accounts": [
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "user"
        },
        {
          "name": "triggerConditions",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "user"
              }
            ]
          }
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": []
    },
    {
      "name": "syncUserConditions",
      "docs": [
        "Grow a zero-copy account to the size this program build compiles in",
        "for its type (resolved from the account discriminator). The migration",
        "crank after an upgrade that appends fields to an account struct; no-op",
        "when already at size. Payer covers the rent-exempt shortfall (auth:",
        "`AccountExtension` hot key, or warm/cold admin). See",
        "`docs/ACCOUNT-EXTENSION.md`.",
        "Derive a user's whole relay condition block — liquidation",
        "thresholds and trigger watches — in one pass.",
        "",
        "The default way to sync a user. `sync_liq_conditions` and",
        "`sync_trigger_conditions` remain for callers that genuinely want",
        "one half (a user with orders and no positions needs no thresholds),",
        "but both write the same account, so calling them in sequence just",
        "classifies the same `remaining_accounts` twice."
      ],
      "discriminator": [
        25,
        90,
        224,
        168,
        223,
        46,
        67,
        255
      ],
      "accounts": [
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "state",
          "docs": [
            "Read for the fee rails the sync's own keeper payment is priced from."
          ]
        },
        {
          "name": "user"
        },
        {
          "name": "userConditions",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "user"
              }
            ]
          }
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "args",
          "type": {
            "defined": {
              "name": "syncLiqConditionsArgs"
            }
          }
        }
      ]
    },
    {
      "name": "transferDeposit",
      "discriminator": [
        20,
        20,
        147,
        223,
        41,
        63,
        204,
        111
      ],
      "accounts": [
        {
          "name": "fromUser",
          "writable": true
        },
        {
          "name": "toUser",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "fromUser",
            "toUser",
            "userStats"
          ]
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarketVault",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "transferDepositByDelegate",
      "discriminator": [
        141,
        171,
        241,
        161,
        17,
        31,
        135,
        29
      ],
      "accounts": [
        {
          "name": "fromUser",
          "writable": true
        },
        {
          "name": "toUser",
          "writable": true
        },
        {
          "name": "userStats"
        },
        {
          "name": "delegate",
          "signer": true,
          "relations": [
            "fromUser",
            "toUser"
          ]
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarketVault",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "u64"
        },
        {
          "name": "equityFloorDelta",
          "type": "u64"
        }
      ]
    },
    {
      "name": "transferFeeAndPnlPool",
      "discriminator": [
        167,
        110,
        96,
        211,
        215,
        250,
        115,
        39
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "perpMarketWithFeePool",
          "writable": true
        },
        {
          "name": "perpMarketWithPnlPool",
          "writable": true
        },
        {
          "name": "spotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "amount",
          "type": "u64"
        },
        {
          "name": "direction",
          "type": {
            "defined": {
              "name": "transferFeeAndPnlPoolDirection"
            }
          }
        }
      ]
    },
    {
      "name": "transferIsolatedPerpPositionDeposit",
      "discriminator": [
        201,
        131,
        242,
        228,
        85,
        226,
        70,
        237
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarketVault",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "spotMarketIndex"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "spotMarketIndex",
          "type": "u16"
        },
        {
          "name": "perpMarketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "i64"
        }
      ]
    },
    {
      "name": "transferPerpPosition",
      "discriminator": [
        23,
        172,
        188,
        168,
        134,
        210,
        3,
        108
      ],
      "accounts": [
        {
          "name": "fromUser",
          "writable": true
        },
        {
          "name": "toUser",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "state"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": {
            "option": "i64"
          }
        }
      ]
    },
    {
      "name": "transferPools",
      "discriminator": [
        197,
        103,
        154,
        25,
        107,
        90,
        60,
        94
      ],
      "accounts": [
        {
          "name": "fromUser",
          "writable": true
        },
        {
          "name": "toUser",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "fromUser",
            "toUser",
            "userStats"
          ]
        },
        {
          "name": "state"
        },
        {
          "name": "depositFromSpotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "depositFromMarketIndex"
              }
            ]
          }
        },
        {
          "name": "depositToSpotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "depositToMarketIndex"
              }
            ]
          }
        },
        {
          "name": "borrowFromSpotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "borrowFromMarketIndex"
              }
            ]
          }
        },
        {
          "name": "borrowToSpotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "borrowToMarketIndex"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        }
      ],
      "args": [
        {
          "name": "depositFromMarketIndex",
          "type": "u16"
        },
        {
          "name": "depositToMarketIndex",
          "type": "u16"
        },
        {
          "name": "borrowFromMarketIndex",
          "type": "u16"
        },
        {
          "name": "borrowToMarketIndex",
          "type": "u16"
        },
        {
          "name": "depositAmount",
          "type": {
            "option": "u64"
          }
        },
        {
          "name": "borrowAmount",
          "type": {
            "option": "u64"
          }
        }
      ]
    },
    {
      "name": "triggerClobOrder",
      "docs": [
        "Crank an armed trigger-limit order onto the market's CLOB once its",
        "trigger condition is met (permissionless; keeper earns the flat",
        "reward from the user). Stop-markets go through `trigger_order`."
      ],
      "discriminator": [
        4,
        206,
        255,
        121,
        250,
        102,
        163,
        14
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "docs": [
            "program-keeper mode (protocol `User` as filler, relay turners) it is",
            "only the lamport payout target and no signature is required."
          ],
          "writable": true
        },
        {
          "name": "filler",
          "writable": true
        },
        {
          "name": "fillerStats",
          "writable": true
        },
        {
          "name": "user",
          "docs": [
            "The owner of the armed trigger order."
          ],
          "writable": true
        },
        {
          "name": "userStats",
          "docs": [
            "Read for the authority-wide equity breaker in the margin gate."
          ]
        },
        {
          "name": "quoter",
          "docs": [
            "The market's CLOB registry entry — placement is only allowed on a",
            "vetted book, same as a direct `place_clob_order`."
          ]
        },
        {
          "name": "clobMarket",
          "docs": [
            "accounts in the handler."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "set to. Deliberately not the vault authority: signer privilege is",
            "inherited by a callee, so the key velocity hands an external program",
            "must be the authority on nothing."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "Expiry-hint host, same optional contract as `place_clob_order`."
          ],
          "writable": true,
          "optional": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "triggerConditions",
          "docs": [
            "The user's relay trigger conditions: the fired slot is released so",
            "its level-triggered wake goes quiet. Optional, like everything else",
            "on the relay side."
          ],
          "writable": true,
          "optional": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "user"
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "orderId",
          "type": "u32"
        }
      ]
    },
    {
      "name": "triggerOrder",
      "discriminator": [
        63,
        112,
        51,
        233,
        232,
        47,
        240,
        199
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "docs": [
            "program-keeper mode (protocol `User` as filler, relay turners) it is",
            "only the lamport payout target and no signature is required."
          ],
          "writable": true
        },
        {
          "name": "filler",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "triggerConditions",
          "docs": [
            "The user's relay trigger conditions: the fired slot is released so",
            "its level-triggered wake goes quiet. Optional — keepers on markets",
            "(or users) without relay plumbing crank exactly as before."
          ],
          "writable": true,
          "optional": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "user"
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "The fired market's crank conditions — the reservoir that pays the",
            "keeper in program-keeper mode (validated against the order's market",
            "in the handler). Required in program-keeper mode."
          ],
          "writable": true,
          "optional": true
        }
      ],
      "args": [
        {
          "name": "orderId",
          "type": "u32"
        }
      ]
    },
    {
      "name": "tripEquityFloorBreaker",
      "discriminator": [
        133,
        184,
        25,
        80,
        193,
        52,
        162,
        249
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "keeper",
          "docs": [
            "Any signer may trip the breaker; the proof is the margin calculation."
          ],
          "signer": true
        },
        {
          "name": "user"
        },
        {
          "name": "userStats",
          "writable": true
        }
      ],
      "args": []
    },
    {
      "name": "updateAdmin",
      "discriminator": [
        161,
        176,
        40,
        213,
        60,
        184,
        179,
        228
      ],
      "accounts": [
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "admin",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "admin",
          "type": "pubkey"
        }
      ]
    },
    {
      "name": "updateAmmCache",
      "discriminator": [
        88,
        4,
        63,
        94,
        83,
        224,
        255,
        130
      ],
      "accounts": [
        {
          "name": "keeper",
          "writable": true,
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "ammCache",
          "writable": true
        },
        {
          "name": "quoteMarket",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        }
      ],
      "args": []
    },
    {
      "name": "updateAmmConstituentMappingData",
      "discriminator": [
        84,
        70,
        33,
        167,
        133,
        107,
        59,
        24
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "lpPool"
        },
        {
          "name": "ammConstituentMapping",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  65,
                  77,
                  77,
                  95,
                  77,
                  65,
                  80
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        },
        {
          "name": "state"
        }
      ],
      "args": [
        {
          "name": "ammConstituentMappingData",
          "type": {
            "vec": {
              "defined": {
                "name": "addAmmConstituentMappingDatum"
              }
            }
          }
        }
      ]
    },
    {
      "name": "updateAmmJitIntensity",
      "discriminator": [
        181,
        191,
        53,
        109,
        166,
        249,
        55,
        142
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "ammJitIntensity",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateAmms",
      "discriminator": [
        201,
        106,
        217,
        253,
        4,
        175,
        228,
        97
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "marketIndexes",
          "type": {
            "vec": "u16"
          }
        }
      ]
    },
    {
      "name": "updateConstituentCorrelationData",
      "discriminator": [
        79,
        14,
        19,
        73,
        221,
        106,
        62,
        109
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "lpPool"
        },
        {
          "name": "constituentCorrelations",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  111,
                  110,
                  115,
                  116,
                  105,
                  116,
                  117,
                  101,
                  110,
                  116,
                  95,
                  99,
                  111,
                  114,
                  114,
                  101,
                  108,
                  97,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "state"
        }
      ],
      "args": [
        {
          "name": "index1",
          "type": "u16"
        },
        {
          "name": "index2",
          "type": "u16"
        },
        {
          "name": "correlation",
          "type": "i64"
        }
      ]
    },
    {
      "name": "updateConstituentOracleInfo",
      "discriminator": [
        198,
        117,
        231,
        250,
        147,
        33,
        127,
        161
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "keeper",
          "writable": true,
          "signer": true
        },
        {
          "name": "constituent",
          "writable": true
        },
        {
          "name": "spotMarket"
        },
        {
          "name": "oracle"
        }
      ],
      "args": []
    },
    {
      "name": "updateConstituentParams",
      "discriminator": [
        238,
        130,
        122,
        31,
        12,
        104,
        192,
        122
      ],
      "accounts": [
        {
          "name": "lpPool"
        },
        {
          "name": "constituentTargetBase",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  111,
                  110,
                  115,
                  116,
                  105,
                  116,
                  117,
                  101,
                  110,
                  116,
                  95,
                  116,
                  97,
                  114,
                  103,
                  101,
                  116,
                  95,
                  98,
                  97,
                  115,
                  101,
                  95,
                  115,
                  101,
                  101,
                  100
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              }
            ]
          }
        },
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "constituent",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "constituentParams",
          "type": {
            "defined": {
              "name": "constituentParams"
            }
          }
        }
      ]
    },
    {
      "name": "updateConstituentPausedOperations",
      "discriminator": [
        185,
        122,
        153,
        191,
        131,
        177,
        132,
        208
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "constituent",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "pausedOperations",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateConstituentStatus",
      "discriminator": [
        76,
        159,
        211,
        239,
        182,
        214,
        6,
        15
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "constituent",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "newStatus",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateCrankTreasury",
      "docs": [
        "Set the two levels a market's crank reservoir is held between, counted",
        "in that market's most expensive crank so one setting fits every market.",
        "Markets take a new watermark on their next attach."
      ],
      "discriminator": [
        18,
        218,
        78,
        149,
        45,
        183,
        2,
        8
      ],
      "accounts": [
        {
          "name": "treasury",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  116,
                  114,
                  101,
                  97,
                  115,
                  117,
                  114,
                  121
                ]
              }
            ]
          }
        },
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        }
      ],
      "args": [
        {
          "name": "refillTargetCranks",
          "type": "u16"
        },
        {
          "name": "refillWatermarkCranks",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updateDiscountMint",
      "discriminator": [
        32,
        252,
        122,
        211,
        66,
        31,
        47,
        241
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "discountMint",
          "type": "pubkey"
        }
      ]
    },
    {
      "name": "updateExchangeStatus",
      "discriminator": [
        83,
        160,
        252,
        250,
        129,
        116,
        49,
        223
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "exchangeStatus",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateFeatureBitFlagsBuilderCodes",
      "discriminator": [
        1,
        128,
        177,
        51,
        173,
        45,
        11,
        102
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "enable",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateFeatureBitFlagsMedianTriggerPrice",
      "discriminator": [
        64,
        185,
        221,
        45,
        87,
        147,
        12,
        19
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "enable",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateFeatureBitFlagsMintRedeemLpPool",
      "discriminator": [
        26,
        11,
        142,
        122,
        206,
        159,
        9,
        45
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "enable",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateFeatureBitFlagsMmOracle",
      "discriminator": [
        218,
        134,
        33,
        186,
        231,
        59,
        130,
        149
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "enable",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateFeatureBitFlagsSettleLpPool",
      "discriminator": [
        186,
        28,
        78,
        230,
        155,
        83,
        242,
        26
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "enable",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateFeatureBitFlagsSwapLpPool",
      "discriminator": [
        83,
        16,
        150,
        12,
        102,
        3,
        22,
        58
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "enable",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateFeatureBitFlagsVammMakerRebate",
      "discriminator": [
        237,
        132,
        7,
        255,
        116,
        155,
        5,
        119
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "enable",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateFundingRate",
      "discriminator": [
        201,
        178,
        116,
        212,
        166,
        144,
        72,
        238
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "oracle"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updateHotAdmin",
      "discriminator": [
        162,
        199,
        182,
        64,
        214,
        244,
        195,
        30
      ],
      "accounts": [
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "admin",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "role",
          "type": {
            "defined": {
              "name": "hotRole"
            }
          }
        },
        {
          "name": "newPubkey",
          "type": "pubkey"
        }
      ]
    },
    {
      "name": "updateInitialAmmCacheInfo",
      "discriminator": [
        157,
        210,
        109,
        67,
        212,
        170,
        12,
        107
      ],
      "accounts": [
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "ammCache",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  97,
                  109,
                  109,
                  95,
                  99,
                  97,
                  99,
                  104,
                  101,
                  95,
                  115,
                  101,
                  101,
                  100
                ]
              }
            ]
          }
        }
      ],
      "args": []
    },
    {
      "name": "updateInitialPctToLiquidate",
      "discriminator": [
        210,
        133,
        225,
        128,
        194,
        50,
        13,
        109
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "initialPctToLiquidate",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updateInsuranceFundUnstakingPeriod",
      "discriminator": [
        44,
        69,
        43,
        226,
        204,
        223,
        202,
        52
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "insuranceFundUnstakingPeriod",
          "type": "i64"
        }
      ]
    },
    {
      "name": "updateK",
      "discriminator": [
        72,
        98,
        9,
        139,
        129,
        229,
        172,
        56
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "oracle"
        }
      ],
      "args": [
        {
          "name": "sqrtK",
          "type": "u128"
        }
      ]
    },
    {
      "name": "updateLiquidationCrankReimbursement",
      "docs": [
        "Set what the protocol spends getting a liquidation cranked, and the",
        "spot market whose oracle prices it in SOL."
      ],
      "discriminator": [
        228,
        33,
        165,
        153,
        37,
        218,
        237,
        252
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "shareBps",
          "type": "u16"
        },
        {
          "name": "solSpotMarketIndex",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updateLiquidationDuration",
      "discriminator": [
        28,
        154,
        20,
        249,
        102,
        192,
        73,
        71
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "liquidationDuration",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateLiquidationMarginBufferRatio",
      "discriminator": [
        132,
        224,
        243,
        160,
        154,
        82,
        97,
        215
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "liquidationMarginBufferRatio",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updateLpConstituentTargetBase",
      "discriminator": [
        157,
        65,
        50,
        207,
        59,
        236,
        161,
        110
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "keeper",
          "writable": true,
          "signer": true
        },
        {
          "name": "ammConstituentMapping"
        },
        {
          "name": "constituentTargetBase",
          "writable": true
        },
        {
          "name": "ammCache"
        },
        {
          "name": "lpPool"
        }
      ],
      "args": []
    },
    {
      "name": "updateLpPoolAum",
      "discriminator": [
        88,
        113,
        137,
        206,
        246,
        247,
        171,
        142
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "keeper",
          "writable": true,
          "signer": true
        },
        {
          "name": "lpPool",
          "writable": true
        },
        {
          "name": "constituentTargetBase",
          "writable": true
        },
        {
          "name": "ammCache",
          "writable": true
        }
      ],
      "args": []
    },
    {
      "name": "updateLpPoolParams",
      "discriminator": [
        217,
        92,
        2,
        255,
        27,
        167,
        178,
        81
      ],
      "accounts": [
        {
          "name": "lpPool",
          "writable": true
        },
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state"
        }
      ],
      "args": [
        {
          "name": "lpPoolParams",
          "type": {
            "defined": {
              "name": "lpPoolParams"
            }
          }
        }
      ]
    },
    {
      "name": "updateOracleGuardRails",
      "discriminator": [
        131,
        112,
        10,
        59,
        32,
        54,
        40,
        164
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "oracleGuardRails",
          "type": {
            "defined": {
              "name": "oracleGuardRails"
            }
          }
        }
      ]
    },
    {
      "name": "updatePauseAdmin",
      "discriminator": [
        25,
        157,
        60,
        228,
        42,
        96,
        133,
        158
      ],
      "accounts": [
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "admin",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "newPauseAdmin",
          "type": "pubkey"
        }
      ]
    },
    {
      "name": "updatePerpAuctionDuration",
      "discriminator": [
        126,
        110,
        52,
        174,
        30,
        206,
        215,
        90
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "minPerpAuctionDuration",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updatePerpBidAskTwap",
      "discriminator": [
        247,
        23,
        255,
        65,
        212,
        90,
        221,
        194
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "oracle"
        },
        {
          "name": "keeperStats"
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "keeperStats"
          ]
        }
      ],
      "args": []
    },
    {
      "name": "updatePerpFeeStructure",
      "discriminator": [
        23,
        178,
        111,
        203,
        73,
        22,
        140,
        75
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "feeStructure",
          "type": {
            "defined": {
              "name": "feeStructure"
            }
          }
        }
      ]
    },
    {
      "name": "updatePerpMarketAmmOracleTwap",
      "discriminator": [
        241,
        74,
        114,
        123,
        206,
        153,
        24,
        202
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "oracle"
        },
        {
          "name": "admin",
          "signer": true
        }
      ],
      "args": []
    },
    {
      "name": "updatePerpMarketAmmSpreadAdjustment",
      "discriminator": [
        155,
        195,
        149,
        43,
        220,
        82,
        173,
        205
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "ammSpreadAdjustment",
          "type": "i8"
        },
        {
          "name": "ammInventorySpreadAdjustment",
          "type": "i8"
        },
        {
          "name": "referencePriceOffset",
          "type": "i32"
        }
      ]
    },
    {
      "name": "updatePerpMarketAmmSummaryStats",
      "discriminator": [
        122,
        101,
        249,
        238,
        209,
        9,
        241,
        245
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "spotMarket",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "perpMarket"
              }
            ]
          }
        },
        {
          "name": "oracle"
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "updatePerpMarketSummaryStatsParams"
            }
          }
        }
      ]
    },
    {
      "name": "updatePerpMarketBankruptcyIfFloorPct",
      "discriminator": [
        192,
        2,
        229,
        220,
        243,
        115,
        121,
        84
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "bankruptcyIfFloorPct",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updatePerpMarketBaseSpread",
      "discriminator": [
        71,
        95,
        84,
        168,
        9,
        157,
        198,
        65
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "baseSpread",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updatePerpMarketClobQuoter",
      "discriminator": [
        210,
        80,
        79,
        140,
        168,
        8,
        27,
        243
      ],
      "accounts": [
        {
          "name": "admin",
          "docs": [
            "Also pays the conditions account's rent on first attach."
          ],
          "writable": true,
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "quoter"
        },
        {
          "name": "clobMarket",
          "docs": [
            "accounts in the handler. Writable because the attach registers",
            "velocity's resolvers on the book itself: the wakes for an expiry, an",
            "activation, a side at its cap and a crossed book are facts about this",
            "account, so the conditions that watch for them live on it."
          ],
          "writable": true
        },
        {
          "name": "clobProgram"
        },
        {
          "name": "quoterSigner",
          "docs": [
            "set to, and therefore the only key that may register its cranks."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  113,
                  117,
                  111,
                  116,
                  101,
                  114,
                  95,
                  115,
                  105,
                  103,
                  110,
                  101,
                  114
                ]
              }
            ]
          }
        },
        {
          "name": "crankConditions",
          "docs": [
            "The market's relay conditions + keeper reservoir, stood up (or",
            "re-priced) as part of the attach so a new market needs no separate",
            "crank ceremony."
          ],
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  108,
                  111,
                  98,
                  95,
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  99,
                  111,
                  110,
                  100,
                  105,
                  116,
                  105,
                  111,
                  110,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "perpMarket"
              }
            ]
          }
        },
        {
          "name": "treasury",
          "docs": [
            "Read-only: the levels a reservoir is held between are the treasury's",
            "setting, and the low one is resolved onto this market here."
          ],
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  116,
                  114,
                  101,
                  97,
                  115,
                  117,
                  114,
                  121
                ]
              }
            ]
          }
        },
        {
          "name": "rent",
          "address": "SysvarRent111111111111111111111111111111111"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        }
      ],
      "args": [
        {
          "name": "crankCostUnits",
          "type": {
            "defined": {
              "name": "crankCostUnitsV0"
            }
          }
        },
        {
          "name": "expireFallbackSlots",
          "type": "u64"
        },
        {
          "name": "minCrossSurplus",
          "type": "u64"
        }
      ]
    },
    {
      "name": "updatePerpMarketConcentrationCoef",
      "discriminator": [
        24,
        78,
        232,
        126,
        169,
        176,
        230,
        16
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "concentrationScale",
          "type": "u128"
        }
      ]
    },
    {
      "name": "updatePerpMarketConfig",
      "discriminator": [
        134,
        90,
        6,
        34,
        80,
        160,
        94,
        245
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "marketConfig",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updatePerpMarketContractTier",
      "discriminator": [
        236,
        128,
        15,
        95,
        203,
        214,
        68,
        117
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "contractTier",
          "type": {
            "defined": {
              "name": "contractTier"
            }
          }
        }
      ]
    },
    {
      "name": "updatePerpMarketCurveUpdateIntensity",
      "discriminator": [
        50,
        131,
        6,
        156,
        226,
        231,
        189,
        72
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "curveUpdateIntensity",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updatePerpMarketExpiry",
      "discriminator": [
        44,
        221,
        227,
        151,
        131,
        140,
        22,
        110
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "expiryTs",
          "type": "i64"
        }
      ]
    },
    {
      "name": "updatePerpMarketFeeAdjustment",
      "discriminator": [
        194,
        174,
        87,
        102,
        43,
        148,
        32,
        112
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "feeAdjustment",
          "type": "i16"
        }
      ]
    },
    {
      "name": "updatePerpMarketFeePoolBufferTarget",
      "discriminator": [
        125,
        234,
        40,
        44,
        91,
        26,
        231,
        177
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "feePoolBufferTarget",
          "type": "u64"
        }
      ]
    },
    {
      "name": "updatePerpMarketFundingBiasSensitivity",
      "discriminator": [
        143,
        62,
        234,
        145,
        184,
        237,
        110,
        116
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "fundingBiasSensitivity",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updatePerpMarketFundingDeadZone",
      "discriminator": [
        249,
        58,
        136,
        96,
        2,
        116,
        111,
        127
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "fundingClampThreshold",
          "type": "u32"
        },
        {
          "name": "fundingRampSlope",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updatePerpMarketFundingPeriod",
      "discriminator": [
        171,
        161,
        69,
        91,
        129,
        139,
        161,
        28
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "fundingPeriod",
          "type": "i64"
        }
      ]
    },
    {
      "name": "updatePerpMarketImfFactor",
      "discriminator": [
        207,
        194,
        56,
        132,
        35,
        67,
        71,
        244
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "imfFactor",
          "type": "u32"
        },
        {
          "name": "unrealizedPnlImfFactor",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updatePerpMarketLiquidationFee",
      "discriminator": [
        90,
        137,
        9,
        145,
        41,
        8,
        148,
        117
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "liquidatorFee",
          "type": "u32"
        },
        {
          "name": "ifLiquidationFee",
          "type": "u32"
        },
        {
          "name": "protocolLiquidationFee",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updatePerpMarketLpPoolFeeTransferScalar",
      "discriminator": [
        94,
        228,
        237,
        109,
        100,
        185,
        4,
        81
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "optionalLpFeeTransferScalar",
          "type": {
            "option": "u8"
          }
        },
        {
          "name": "optionalLpNetPnlTransferScalar",
          "type": {
            "option": "u8"
          }
        }
      ]
    },
    {
      "name": "updatePerpMarketLpPoolId",
      "discriminator": [
        119,
        208,
        154,
        88,
        165,
        92,
        21,
        188
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "lpPoolId",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updatePerpMarketLpPoolPausedOperations",
      "discriminator": [
        181,
        94,
        93,
        146,
        51,
        89,
        32,
        135
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "lpPausedOperations",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updatePerpMarketLpPoolStatus",
      "discriminator": [
        67,
        6,
        252,
        61,
        54,
        88,
        89,
        233
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "ammCache",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  97,
                  109,
                  109,
                  95,
                  99,
                  97,
                  99,
                  104,
                  101,
                  95,
                  115,
                  101,
                  101,
                  100
                ]
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "lpStatus",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updatePerpMarketMarginRatio",
      "discriminator": [
        130,
        173,
        107,
        45,
        119,
        105,
        26,
        113
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "marginRatioInitial",
          "type": "u32"
        },
        {
          "name": "marginRatioMaintenance",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updatePerpMarketMaxFillReserveFraction",
      "discriminator": [
        19,
        172,
        114,
        154,
        42,
        135,
        161,
        133
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "maxFillReserveFraction",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updatePerpMarketMaxImbalances",
      "discriminator": [
        15,
        206,
        73,
        133,
        60,
        8,
        86,
        89
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "unrealizedMaxImbalance",
          "type": "u64"
        },
        {
          "name": "maxRevenueWithdrawPerPeriod",
          "type": "u64"
        },
        {
          "name": "quoteMaxInsurance",
          "type": "u64"
        }
      ]
    },
    {
      "name": "updatePerpMarketMaxOpenInterest",
      "discriminator": [
        194,
        79,
        149,
        224,
        246,
        102,
        186,
        140
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "maxOpenInterest",
          "type": "u128"
        }
      ]
    },
    {
      "name": "updatePerpMarketMaxSlippageRatio",
      "discriminator": [
        235,
        37,
        40,
        196,
        70,
        146,
        54,
        201
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "maxSlippageRatio",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updatePerpMarketMaxSpread",
      "discriminator": [
        80,
        252,
        122,
        62,
        40,
        218,
        91,
        100
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "maxSpread",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updatePerpMarketMinOrderSize",
      "discriminator": [
        226,
        74,
        5,
        89,
        108,
        223,
        46,
        141
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "orderSize",
          "type": "u64"
        }
      ]
    },
    {
      "name": "updatePerpMarketName",
      "discriminator": [
        211,
        31,
        21,
        210,
        64,
        108,
        66,
        201
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "name",
          "type": {
            "array": [
              "u8",
              32
            ]
          }
        }
      ]
    },
    {
      "name": "updatePerpMarketNumberOfUsers",
      "discriminator": [
        35,
        62,
        144,
        177,
        180,
        62,
        215,
        196
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "numberOfUsers",
          "type": {
            "option": "u32"
          }
        },
        {
          "name": "numberOfUsersWithBase",
          "type": {
            "option": "u32"
          }
        }
      ]
    },
    {
      "name": "updatePerpMarketOracle",
      "discriminator": [
        182,
        113,
        111,
        160,
        67,
        174,
        89,
        191
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "oracle"
        },
        {
          "name": "oldOracle"
        },
        {
          "name": "ammCache",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  97,
                  109,
                  109,
                  95,
                  99,
                  97,
                  99,
                  104,
                  101,
                  95,
                  115,
                  101,
                  101,
                  100
                ]
              }
            ]
          }
        }
      ],
      "args": [
        {
          "name": "oracle",
          "type": "pubkey"
        },
        {
          "name": "oracleSource",
          "type": {
            "defined": {
              "name": "oracleSource"
            }
          }
        },
        {
          "name": "skipInvariantCheck",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updatePerpMarketOracleLowRiskSlotDelayOverride",
      "discriminator": [
        124,
        108,
        147,
        229,
        109,
        117,
        123,
        3
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "oracleLowRiskSlotDelayOverride",
          "type": "i8"
        }
      ]
    },
    {
      "name": "updatePerpMarketOracleSlotDelayOverride",
      "discriminator": [
        165,
        91,
        239,
        227,
        63,
        172,
        227,
        8
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "oracleSlotDelayOverride",
          "type": "i8"
        }
      ]
    },
    {
      "name": "updatePerpMarketPausedOperations",
      "discriminator": [
        53,
        16,
        136,
        132,
        30,
        220,
        121,
        85
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "pausedOperations",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updatePerpMarketPnlPool",
      "discriminator": [
        50,
        202,
        249,
        224,
        166,
        184,
        13,
        143
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "spotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "updatePerpMarketReferencePriceOffsetDeadbandPct",
      "discriminator": [
        214,
        73,
        166,
        11,
        218,
        76,
        110,
        163
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "referencePriceOffsetDeadbandPct",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updatePerpMarketStatus",
      "discriminator": [
        71,
        201,
        175,
        122,
        255,
        207,
        196,
        207
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "status",
          "type": {
            "defined": {
              "name": "marketStatus"
            }
          }
        }
      ]
    },
    {
      "name": "updatePerpMarketStepSizeAndTickSize",
      "discriminator": [
        231,
        255,
        97,
        25,
        146,
        139,
        174,
        4
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "stepSize",
          "type": "u64"
        },
        {
          "name": "tickSize",
          "type": "u64"
        }
      ]
    },
    {
      "name": "updatePerpMarketTakerFeeAddon",
      "discriminator": [
        53,
        22,
        191,
        15,
        62,
        150,
        36,
        203
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "takerFeeAddonTenthBps",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updatePerpMarketUnrealizedAssetWeight",
      "discriminator": [
        135,
        132,
        205,
        165,
        109,
        150,
        166,
        106
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "unrealizedInitialAssetWeight",
          "type": "u32"
        },
        {
          "name": "unrealizedMaintenanceAssetWeight",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updatePrelaunchOracle",
      "discriminator": [
        220,
        132,
        27,
        27,
        233,
        220,
        61,
        219
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "perpMarket"
        },
        {
          "name": "oracle",
          "writable": true
        }
      ],
      "args": []
    },
    {
      "name": "updatePrelaunchOracleParams",
      "discriminator": [
        98,
        205,
        147,
        243,
        18,
        75,
        83,
        207
      ],
      "accounts": [
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "prelaunchOracle",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  112,
                  114,
                  101,
                  108,
                  97,
                  117,
                  110,
                  99,
                  104,
                  95,
                  111,
                  114,
                  97,
                  99,
                  108,
                  101
                ]
              },
              {
                "kind": "arg",
                "path": "params.perp_market_index"
              }
            ]
          }
        },
        {
          "name": "perpMarket",
          "writable": true
        },
        {
          "name": "state"
        }
      ],
      "args": [
        {
          "name": "params",
          "type": {
            "defined": {
              "name": "prelaunchOracleParams"
            }
          }
        }
      ]
    },
    {
      "name": "updatePromoFeeTier",
      "discriminator": [
        104,
        57,
        241,
        162,
        69,
        198,
        5,
        175
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "promoFeeTier",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateProtocolFeeRecipient",
      "docs": [
        "Cold-only: set the treasury protocol fees may be withdrawn to.",
        "Perp (quote) and spot (per-market token) recipients are configured",
        "independently via `market_type`."
      ],
      "discriminator": [
        213,
        60,
        21,
        106,
        42,
        67,
        60,
        162
      ],
      "accounts": [
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "admin",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "protocolFeeRecipient",
          "type": "pubkey"
        },
        {
          "name": "marketType",
          "type": {
            "defined": {
              "name": "marketType"
            }
          }
        }
      ]
    },
    {
      "name": "updateQuoterAccounts",
      "discriminator": [
        60,
        210,
        110,
        134,
        146,
        21,
        140,
        220
      ],
      "accounts": [
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "quoter",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "args",
          "type": {
            "defined": {
              "name": "updateQuoterAccountsArgs"
            }
          }
        }
      ]
    },
    {
      "name": "updateQuoterActive",
      "discriminator": [
        62,
        158,
        7,
        155,
        123,
        213,
        219,
        237
      ],
      "accounts": [
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "quoter",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "active",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateQuoterApproved",
      "discriminator": [
        170,
        62,
        247,
        58,
        166,
        107,
        153,
        92
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "quoter",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "approved",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateQuoterConfig",
      "discriminator": [
        210,
        218,
        133,
        195,
        87,
        215,
        7,
        209
      ],
      "accounts": [
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "quoter",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "args",
          "type": {
            "defined": {
              "name": "updateQuoterConfigArgs"
            }
          }
        }
      ]
    },
    {
      "name": "updateQuoterPriority",
      "discriminator": [
        192,
        118,
        16,
        53,
        87,
        232,
        85,
        234
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "quoter",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "priority",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateQuoterWatch",
      "discriminator": [
        97,
        137,
        30,
        49,
        137,
        246,
        203,
        198
      ],
      "accounts": [
        {
          "name": "authority",
          "docs": [
            "The entry's own authority — the quoted user's wallet for Custom",
            "entries."
          ],
          "signer": true,
          "relations": [
            "quoter"
          ]
        },
        {
          "name": "quoter",
          "writable": true
        },
        {
          "name": "watchAccount",
          "docs": [
            "quoter's own state account; not otherwise constrained (the admin",
            "vets it, and a wrong watch only costs the maker latency)."
          ]
        }
      ],
      "args": [
        {
          "name": "args",
          "type": {
            "defined": {
              "name": "updateQuoterWatchArgs"
            }
          }
        }
      ]
    },
    {
      "name": "updateSolvencyStatus",
      "discriminator": [
        81,
        136,
        15,
        6,
        24,
        165,
        44,
        133
      ],
      "accounts": [
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "admin",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "solvencyStatus",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateSpecialUserStatus",
      "discriminator": [
        23,
        237,
        166,
        194,
        38,
        7,
        41,
        45
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "status",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateSpotAuctionDuration",
      "discriminator": [
        182,
        178,
        203,
        72,
        187,
        143,
        157,
        107
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "defaultSpotAuctionDuration",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateSpotFeeStructure",
      "discriminator": [
        97,
        216,
        105,
        131,
        113,
        246,
        142,
        141
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "feeStructure",
          "type": {
            "defined": {
              "name": "feeStructure"
            }
          }
        }
      ]
    },
    {
      "name": "updateSpotMarketAssetTier",
      "discriminator": [
        253,
        209,
        231,
        14,
        242,
        208,
        243,
        130
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "assetTier",
          "type": {
            "defined": {
              "name": "assetTier"
            }
          }
        }
      ]
    },
    {
      "name": "updateSpotMarketBorrowRate",
      "discriminator": [
        71,
        239,
        236,
        153,
        210,
        62,
        254,
        76
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "optimalUtilization",
          "type": "u32"
        },
        {
          "name": "optimalBorrowRate",
          "type": "u32"
        },
        {
          "name": "maxBorrowRate",
          "type": "u32"
        },
        {
          "name": "minBorrowRate",
          "type": {
            "option": "u8"
          }
        }
      ]
    },
    {
      "name": "updateSpotMarketCumulativeInterest",
      "discriminator": [
        39,
        166,
        139,
        243,
        158,
        165,
        155,
        225
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        },
        {
          "name": "oracle"
        },
        {
          "name": "spotMarketVault",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "spotMarket"
              }
            ]
          }
        }
      ],
      "args": []
    },
    {
      "name": "updateSpotMarketDepositCap",
      "discriminator": [
        76,
        21,
        179,
        154,
        28,
        161,
        174,
        107
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "depositGuardThreshold",
          "type": "u64"
        },
        {
          "name": "maxDepositBpsPerDay",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updateSpotMarketExpiry",
      "discriminator": [
        208,
        11,
        211,
        159,
        226,
        24,
        11,
        247
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "expiryTs",
          "type": "i64"
        }
      ]
    },
    {
      "name": "updateSpotMarketFeeAdjustment",
      "discriminator": [
        148,
        182,
        3,
        126,
        157,
        114,
        220,
        99
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "feeAdjustment",
          "type": "i16"
        }
      ]
    },
    {
      "name": "updateSpotMarketIfFactor",
      "discriminator": [
        147,
        30,
        224,
        34,
        18,
        230,
        105,
        4
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "spotMarketIndex",
          "type": "u16"
        },
        {
          "name": "ifFeeFactor",
          "type": "u32"
        },
        {
          "name": "protocolFeeFactor",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updateSpotMarketIfPausedOperations",
      "discriminator": [
        101,
        215,
        79,
        74,
        59,
        41,
        79,
        12
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "pausedOperations",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateSpotMarketLiquidationFee",
      "discriminator": [
        11,
        13,
        255,
        53,
        56,
        136,
        104,
        177
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "liquidatorFee",
          "type": "u32"
        },
        {
          "name": "ifLiquidationFee",
          "type": "u32"
        },
        {
          "name": "protocolLiquidationFee",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updateSpotMarketMarginWeights",
      "discriminator": [
        109,
        33,
        87,
        195,
        255,
        36,
        6,
        81
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "initialAssetWeight",
          "type": "u32"
        },
        {
          "name": "maintenanceAssetWeight",
          "type": "u32"
        },
        {
          "name": "initialLiabilityWeight",
          "type": "u32"
        },
        {
          "name": "maintenanceLiabilityWeight",
          "type": "u32"
        },
        {
          "name": "imfFactor",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updateSpotMarketMaxTokenBorrows",
      "discriminator": [
        57,
        102,
        204,
        212,
        253,
        95,
        13,
        199
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "maxTokenBorrowsFraction",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updateSpotMarketMaxTokenDeposits",
      "discriminator": [
        56,
        191,
        79,
        18,
        26,
        121,
        80,
        208
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "maxTokenDeposits",
          "type": "u64"
        }
      ]
    },
    {
      "name": "updateSpotMarketMinOrderSize",
      "discriminator": [
        93,
        128,
        11,
        119,
        26,
        20,
        181,
        50
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "orderSize",
          "type": "u64"
        }
      ]
    },
    {
      "name": "updateSpotMarketName",
      "discriminator": [
        17,
        208,
        1,
        1,
        162,
        211,
        188,
        224
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "name",
          "type": {
            "array": [
              "u8",
              32
            ]
          }
        }
      ]
    },
    {
      "name": "updateSpotMarketOracle",
      "discriminator": [
        114,
        184,
        102,
        37,
        246,
        186,
        180,
        99
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        },
        {
          "name": "oracle"
        },
        {
          "name": "oldOracle"
        }
      ],
      "args": [
        {
          "name": "oracle",
          "type": "pubkey"
        },
        {
          "name": "oracleSource",
          "type": {
            "defined": {
              "name": "oracleSource"
            }
          }
        },
        {
          "name": "skipInvariantCheck",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateSpotMarketOrdersEnabled",
      "discriminator": [
        190,
        79,
        206,
        15,
        26,
        229,
        229,
        43
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "ordersEnabled",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateSpotMarketPausedOperations",
      "discriminator": [
        100,
        61,
        153,
        81,
        180,
        12,
        6,
        248
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "pausedOperations",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateSpotMarketPoolId",
      "discriminator": [
        22,
        213,
        197,
        160,
        139,
        193,
        81,
        149
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "poolId",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateSpotMarketRevenueSettlePeriod",
      "discriminator": [
        81,
        92,
        126,
        41,
        250,
        225,
        156,
        219
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "revenueSettlePeriod",
          "type": "i64"
        }
      ]
    },
    {
      "name": "updateSpotMarketScaleInitialAssetWeightStart",
      "discriminator": [
        217,
        204,
        204,
        118,
        204,
        130,
        225,
        147
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "scaleInitialAssetWeightStart",
          "type": "u64"
        }
      ]
    },
    {
      "name": "updateSpotMarketStatus",
      "discriminator": [
        78,
        94,
        16,
        188,
        193,
        110,
        231,
        31
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "status",
          "type": {
            "defined": {
              "name": "marketStatus"
            }
          }
        }
      ]
    },
    {
      "name": "updateSpotMarketStepSizeAndTickSize",
      "discriminator": [
        238,
        153,
        137,
        80,
        206,
        59,
        250,
        61
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "stepSize",
          "type": "u64"
        },
        {
          "name": "tickSize",
          "type": "u64"
        }
      ]
    },
    {
      "name": "updateSpotMarketWithdrawCircuitBreaker",
      "discriminator": [
        2,
        97,
        135,
        97,
        117,
        169,
        65,
        223
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "withdrawCircuitBreakerBps",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updateStateMaxInitializeUserFee",
      "discriminator": [
        237,
        225,
        25,
        237,
        193,
        45,
        77,
        97
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "maxInitializeUserFee",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updateStateMaxNumberOfSubAccounts",
      "discriminator": [
        155,
        123,
        214,
        2,
        221,
        166,
        204,
        85
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "maxNumberOfSubAccounts",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updateStateSettlementDuration",
      "discriminator": [
        97,
        68,
        199,
        235,
        131,
        80,
        61,
        173
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "settlementDuration",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updateTransactionFeeRails",
      "discriminator": [
        196,
        239,
        164,
        219,
        198,
        45,
        242,
        11
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "rails",
          "type": {
            "defined": {
              "name": "transactionFeeRails"
            }
          }
        }
      ]
    },
    {
      "name": "updateUserAllowDelegateTransfer",
      "discriminator": [
        235,
        106,
        172,
        39,
        223,
        238,
        167,
        204
      ],
      "accounts": [
        {
          "name": "userStats",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114,
                  95,
                  115,
                  116,
                  97,
                  116,
                  115
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              }
            ]
          }
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "userStats"
          ]
        }
      ],
      "args": [
        {
          "name": "allowDelegateTransfer",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateUserCustomMarginRatio",
      "discriminator": [
        21,
        221,
        140,
        187,
        32,
        129,
        11,
        123
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              },
              {
                "kind": "arg",
                "path": "subAccountId"
              }
            ]
          }
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "subAccountId",
          "type": "u16"
        },
        {
          "name": "marginRatio",
          "type": "u32"
        }
      ]
    },
    {
      "name": "updateUserDelegate",
      "discriminator": [
        139,
        205,
        141,
        141,
        113,
        36,
        94,
        187
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              },
              {
                "kind": "arg",
                "path": "subAccountId"
              }
            ]
          }
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "subAccountId",
          "type": "u16"
        },
        {
          "name": "delegate",
          "type": "pubkey"
        }
      ]
    },
    {
      "name": "updateUserEquityFloor",
      "discriminator": [
        49,
        87,
        139,
        119,
        136,
        239,
        186,
        104
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        }
      ],
      "args": [
        {
          "name": "equityFloor",
          "type": "u64"
        },
        {
          "name": "equityFloorBuffer",
          "type": "u64"
        }
      ]
    },
    {
      "name": "updateUserIdle",
      "discriminator": [
        253,
        133,
        67,
        22,
        103,
        161,
        20,
        100
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "filler",
          "writable": true
        },
        {
          "name": "user",
          "writable": true
        }
      ],
      "args": []
    },
    {
      "name": "updateUserMarginTradingEnabled",
      "discriminator": [
        194,
        92,
        204,
        223,
        246,
        188,
        31,
        203
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              },
              {
                "kind": "arg",
                "path": "subAccountId"
              }
            ]
          }
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "subAccountId",
          "type": "u16"
        },
        {
          "name": "marginTradingEnabled",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateUserName",
      "discriminator": [
        135,
        25,
        185,
        56,
        165,
        53,
        34,
        136
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              },
              {
                "kind": "arg",
                "path": "subAccountId"
              }
            ]
          }
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "subAccountId",
          "type": "u16"
        },
        {
          "name": "name",
          "type": {
            "array": [
              "u8",
              32
            ]
          }
        }
      ]
    },
    {
      "name": "updateUserPerpPositionCustomMarginRatio",
      "discriminator": [
        121,
        137,
        157,
        155,
        89,
        186,
        145,
        113
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "subAccountId",
          "type": "u16"
        },
        {
          "name": "perpMarketIndex",
          "type": "u16"
        },
        {
          "name": "marginRatio",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updateUserPoolId",
      "discriminator": [
        219,
        86,
        73,
        106,
        56,
        218,
        128,
        109
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              },
              {
                "kind": "arg",
                "path": "subAccountId"
              }
            ]
          }
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "subAccountId",
          "type": "u16"
        },
        {
          "name": "poolId",
          "type": "u8"
        }
      ]
    },
    {
      "name": "updateUserQuoteAssetInsuranceStake",
      "discriminator": [
        251,
        101,
        156,
        7,
        2,
        63,
        30,
        23
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        },
        {
          "name": "insuranceFundStake",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "signer",
          "signer": true
        },
        {
          "name": "insuranceFundVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  105,
                  110,
                  115,
                  117,
                  114,
                  97,
                  110,
                  99,
                  101,
                  95,
                  102,
                  117,
                  110,
                  100,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "const",
                "value": [
                  0,
                  0
                ]
              }
            ]
          }
        }
      ],
      "args": []
    },
    {
      "name": "updateUserReduceOnly",
      "discriminator": [
        199,
        71,
        42,
        67,
        144,
        19,
        86,
        109
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              },
              {
                "kind": "arg",
                "path": "subAccountId"
              }
            ]
          }
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "subAccountId",
          "type": "u16"
        },
        {
          "name": "reduceOnly",
          "type": "bool"
        }
      ]
    },
    {
      "name": "updateUserStatsReferrerStatus",
      "discriminator": [
        174,
        154,
        72,
        42,
        191,
        148,
        145,
        205
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "userStats",
          "writable": true
        }
      ],
      "args": []
    },
    {
      "name": "updateUserVaultOwned",
      "docs": [
        "Mark a User as vault-owned (its authority is a vault PDA and its equity",
        "prices vault depositor shares). Set-only and authority-gated: only the",
        "User's authority may call it, and it is CPI'd by the vaults program at",
        "vault init. A vault-owned User is skipped by the revenue-share sweep so a",
        "builder/referral reward can never enter vault NAV (OtterSec #91/#92/#93)."
      ],
      "discriminator": [
        50,
        156,
        218,
        143,
        216,
        94,
        68,
        93
      ],
      "accounts": [
        {
          "name": "user",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  117,
                  115,
                  101,
                  114
                ]
              },
              {
                "kind": "account",
                "path": "authority"
              },
              {
                "kind": "arg",
                "path": "subAccountId"
              }
            ]
          }
        },
        {
          "name": "authority",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "subAccountId",
          "type": "u16"
        }
      ]
    },
    {
      "name": "updateWarmAdmin",
      "discriminator": [
        35,
        244,
        112,
        119,
        5,
        99,
        195,
        204
      ],
      "accounts": [
        {
          "name": "state",
          "writable": true
        },
        {
          "name": "admin",
          "signer": true
        }
      ],
      "args": [
        {
          "name": "newWarmAdmin",
          "type": "pubkey"
        }
      ]
    },
    {
      "name": "updateWithdrawGuardThreshold",
      "discriminator": [
        56,
        18,
        39,
        61,
        155,
        211,
        44,
        133
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "spotMarket",
          "writable": true
        },
        {
          "name": "oracle",
          "relations": [
            "spotMarket"
          ]
        }
      ],
      "args": [
        {
          "name": "withdrawGuardThreshold",
          "type": "u64"
        }
      ]
    },
    {
      "name": "viewLpPoolAddLiquidityFees",
      "discriminator": [
        80,
        66,
        226,
        161,
        70,
        142,
        119,
        84
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "lpPool"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "inMarketMint"
        },
        {
          "name": "inConstituent",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "inMarketIndex"
              }
            ]
          }
        },
        {
          "name": "lpMint"
        },
        {
          "name": "constituentTargetBase"
        }
      ],
      "args": [
        {
          "name": "inMarketIndex",
          "type": "u16"
        },
        {
          "name": "inAmount",
          "type": "u128"
        }
      ]
    },
    {
      "name": "viewLpPoolRemoveLiquidityFees",
      "discriminator": [
        47,
        12,
        9,
        102,
        12,
        226,
        197,
        89
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "lpPool"
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "outMarketMint"
        },
        {
          "name": "outConstituent",
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "inMarketIndex"
              }
            ]
          }
        },
        {
          "name": "lpMint"
        },
        {
          "name": "constituentTargetBase"
        }
      ],
      "args": [
        {
          "name": "inMarketIndex",
          "type": "u16"
        },
        {
          "name": "inAmount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "viewLpPoolSwapFees",
      "discriminator": [
        126,
        189,
        109,
        189,
        170,
        156,
        3,
        46
      ],
      "accounts": [
        {
          "name": "velocitySigner"
        },
        {
          "name": "state"
        },
        {
          "name": "lpPool"
        },
        {
          "name": "constituentTargetBase"
        },
        {
          "name": "constituentCorrelations"
        },
        {
          "name": "constituentInTokenAccount",
          "writable": true
        },
        {
          "name": "constituentOutTokenAccount",
          "writable": true
        },
        {
          "name": "inConstituent",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "inMarketIndex"
              }
            ]
          }
        },
        {
          "name": "outConstituent",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  67,
                  79,
                  78,
                  83,
                  84,
                  73,
                  84,
                  85,
                  69,
                  78,
                  84
                ]
              },
              {
                "kind": "account",
                "path": "lpPool"
              },
              {
                "kind": "arg",
                "path": "outMarketIndex"
              }
            ]
          }
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "inMarketIndex",
          "type": "u16"
        },
        {
          "name": "outMarketIndex",
          "type": "u16"
        },
        {
          "name": "inAmount",
          "type": "u64"
        },
        {
          "name": "inTargetWeight",
          "type": "i64"
        },
        {
          "name": "outTargetWeight",
          "type": "i64"
        }
      ]
    },
    {
      "name": "withdraw",
      "discriminator": [
        183,
        18,
        70,
        156,
        148,
        109,
        161,
        34
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "user",
            "userStats"
          ]
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "userTokenAccount",
          "writable": true
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "u64"
        },
        {
          "name": "reduceOnly",
          "type": "bool"
        }
      ]
    },
    {
      "name": "withdrawCrankTreasury",
      "docs": [
        "Recover lamports from the crank treasury, never below its own rent."
      ],
      "discriminator": [
        7,
        7,
        168,
        31,
        50,
        178,
        211,
        130
      ],
      "accounts": [
        {
          "name": "treasury",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  99,
                  114,
                  97,
                  110,
                  107,
                  95,
                  116,
                  114,
                  101,
                  97,
                  115,
                  117,
                  114,
                  121
                ]
              }
            ]
          }
        },
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "state"
        }
      ],
      "args": [
        {
          "name": "lamports",
          "type": "u64"
        }
      ]
    },
    {
      "name": "withdrawFromIsolatedPerpPosition",
      "discriminator": [
        37,
        92,
        178,
        149,
        140,
        76,
        159,
        135
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "user",
          "writable": true
        },
        {
          "name": "userStats",
          "writable": true
        },
        {
          "name": "authority",
          "signer": true,
          "relations": [
            "user",
            "userStats"
          ]
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "spotMarketIndex"
              }
            ]
          }
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "userTokenAccount",
          "writable": true
        },
        {
          "name": "tokenProgram"
        }
      ],
      "args": [
        {
          "name": "spotMarketIndex",
          "type": "u16"
        },
        {
          "name": "perpMarketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "withdrawFromProgramVault",
      "discriminator": [
        120,
        40,
        183,
        149,
        232,
        18,
        224,
        151
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "admin",
          "writable": true,
          "signer": true
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "constituent",
          "writable": true
        },
        {
          "name": "constituentTokenAccount",
          "writable": true
        },
        {
          "name": "spotMarket",
          "writable": true
        },
        {
          "name": "spotMarketVault",
          "writable": true
        },
        {
          "name": "tokenProgram"
        },
        {
          "name": "mint"
        },
        {
          "name": "oracle"
        }
      ],
      "args": [
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "withdrawProtocolFeesPerp",
      "docs": [
        "Withdraw a perp market's accrued protocol fees (from the quote spot vault)",
        "to `protocol_fee_recipient_perp` (auth: `FeeWithdraw` hot key)."
      ],
      "discriminator": [
        227,
        99,
        23,
        227,
        168,
        217,
        136,
        181
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "perpMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  112,
                  101,
                  114,
                  112,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "quoteSpotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "perpMarket"
              }
            ]
          }
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "account",
                "path": "perpMarket"
              }
            ]
          }
        },
        {
          "name": "mint",
          "relations": [
            "spotMarketVault"
          ]
        },
        {
          "name": "recipient"
        },
        {
          "name": "recipientTokenAccount",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "account",
                "path": "recipient"
              },
              {
                "kind": "account",
                "path": "tokenProgram"
              },
              {
                "kind": "account",
                "path": "mint"
              }
            ],
            "program": {
              "kind": "const",
              "value": [
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
                89
              ]
            }
          }
        },
        {
          "name": "tokenProgram"
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        },
        {
          "name": "associatedTokenProgram",
          "address": "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "withdrawProtocolFeesSpot",
      "docs": [
        "Withdraw a spot market's accrued protocol fees to `protocol_fee_recipient_spot`",
        "(auth: `FeeWithdraw` hot key)."
      ],
      "discriminator": [
        177,
        216,
        30,
        239,
        253,
        177,
        123,
        155
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "spotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "mint",
          "relations": [
            "spotMarketVault"
          ]
        },
        {
          "name": "recipient"
        },
        {
          "name": "recipientTokenAccount",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "account",
                "path": "recipient"
              },
              {
                "kind": "account",
                "path": "tokenProgram"
              },
              {
                "kind": "account",
                "path": "mint"
              }
            ],
            "program": {
              "kind": "const",
              "value": [
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
                89
              ]
            }
          }
        },
        {
          "name": "tokenProgram"
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        },
        {
          "name": "associatedTokenProgram",
          "address": "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "withdrawProtocolUserDeposit",
      "discriminator": [
        148,
        218,
        82,
        85,
        187,
        96,
        44,
        87
      ],
      "accounts": [
        {
          "name": "state"
        },
        {
          "name": "payer",
          "writable": true,
          "signer": true
        },
        {
          "name": "authority",
          "signer": true
        },
        {
          "name": "protocolUser",
          "writable": true
        },
        {
          "name": "spotMarket",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "spotMarketVault",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "const",
                "value": [
                  115,
                  112,
                  111,
                  116,
                  95,
                  109,
                  97,
                  114,
                  107,
                  101,
                  116,
                  95,
                  118,
                  97,
                  117,
                  108,
                  116
                ]
              },
              {
                "kind": "arg",
                "path": "marketIndex"
              }
            ]
          }
        },
        {
          "name": "mint",
          "relations": [
            "spotMarketVault"
          ]
        },
        {
          "name": "recipient"
        },
        {
          "name": "recipientTokenAccount",
          "writable": true,
          "pda": {
            "seeds": [
              {
                "kind": "account",
                "path": "recipient"
              },
              {
                "kind": "account",
                "path": "tokenProgram"
              },
              {
                "kind": "account",
                "path": "mint"
              }
            ],
            "program": {
              "kind": "const",
              "value": [
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
                89
              ]
            }
          }
        },
        {
          "name": "tokenProgram"
        },
        {
          "name": "velocitySigner"
        },
        {
          "name": "systemProgram",
          "address": "11111111111111111111111111111111"
        },
        {
          "name": "associatedTokenProgram",
          "address": "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL"
        }
      ],
      "args": [
        {
          "name": "marketIndex",
          "type": "u16"
        },
        {
          "name": "amount",
          "type": "u64"
        }
      ]
    },
    {
      "name": "zeroMmOracleFields",
      "discriminator": [
        192,
        226,
        39,
        204,
        207,
        120,
        148,
        250
      ],
      "accounts": [
        {
          "name": "admin",
          "signer": true
        },
        {
          "name": "state"
        },
        {
          "name": "perpMarket",
          "writable": true
        }
      ],
      "args": []
    }
  ],
  "accounts": [
    {
      "name": "ammCache",
      "discriminator": [
        213,
        114,
        161,
        56,
        20,
        22,
        2,
        59
      ]
    },
    {
      "name": "ammConstituentMapping",
      "discriminator": [
        254,
        89,
        5,
        173,
        66,
        54,
        214,
        247
      ]
    },
    {
      "name": "clobCrankConditionsV0",
      "discriminator": [
        192,
        236,
        226,
        61,
        136,
        80,
        33,
        74
      ]
    },
    {
      "name": "constituent",
      "discriminator": [
        0,
        61,
        36,
        35,
        177,
        76,
        216,
        205
      ]
    },
    {
      "name": "constituentCorrelations",
      "discriminator": [
        124,
        203,
        115,
        33,
        18,
        162,
        67,
        216
      ]
    },
    {
      "name": "constituentTargetBase",
      "discriminator": [
        255,
        142,
        134,
        71,
        125,
        66,
        198,
        99
      ]
    },
    {
      "name": "crankTreasuryV0",
      "discriminator": [
        50,
        111,
        200,
        27,
        38,
        23,
        240,
        155
      ]
    },
    {
      "name": "insuranceFundStake",
      "discriminator": [
        110,
        202,
        14,
        42,
        95,
        73,
        90,
        95
      ]
    },
    {
      "name": "lpPool",
      "discriminator": [
        228,
        152,
        141,
        224,
        161,
        170,
        11,
        89
      ]
    },
    {
      "name": "perpMarket",
      "discriminator": [
        10,
        223,
        12,
        44,
        107,
        245,
        55,
        247
      ]
    },
    {
      "name": "prelaunchOracle",
      "discriminator": [
        92,
        14,
        139,
        234,
        72,
        244,
        68,
        26
      ]
    },
    {
      "name": "pythLazerOracle",
      "discriminator": [
        159,
        7,
        161,
        249,
        34,
        81,
        121,
        133
      ]
    },
    {
      "name": "quoterCrossConditionsV0",
      "discriminator": [
        72,
        61,
        211,
        139,
        238,
        110,
        226,
        41
      ]
    },
    {
      "name": "quoterV0",
      "discriminator": [
        71,
        216,
        164,
        77,
        121,
        15,
        57,
        124
      ]
    },
    {
      "name": "referrerName",
      "discriminator": [
        105,
        133,
        170,
        110,
        52,
        42,
        28,
        182
      ]
    },
    {
      "name": "relayScratchV0",
      "discriminator": [
        233,
        134,
        29,
        108,
        164,
        128,
        221,
        138
      ]
    },
    {
      "name": "revenueShare",
      "discriminator": [
        55,
        40,
        228,
        7,
        139,
        52,
        180,
        110
      ]
    },
    {
      "name": "revenueShareEscrow",
      "discriminator": [
        98,
        167,
        3,
        46,
        74,
        177,
        173,
        252
      ]
    },
    {
      "name": "routerQuoteBufferV0",
      "discriminator": [
        226,
        115,
        111,
        70,
        54,
        54,
        253,
        75
      ]
    },
    {
      "name": "signedMsgUserOrders",
      "discriminator": [
        70,
        6,
        50,
        248,
        222,
        1,
        143,
        49
      ]
    },
    {
      "name": "signedMsgWsDelegates",
      "discriminator": [
        190,
        115,
        111,
        44,
        216,
        252,
        108,
        85
      ]
    },
    {
      "name": "spotMarket",
      "discriminator": [
        100,
        177,
        8,
        107,
        168,
        65,
        65,
        39
      ]
    },
    {
      "name": "state",
      "discriminator": [
        216,
        146,
        107,
        94,
        104,
        75,
        182,
        177
      ]
    },
    {
      "name": "user",
      "discriminator": [
        159,
        117,
        95,
        227,
        239,
        151,
        58,
        236
      ]
    },
    {
      "name": "userConditionsV0",
      "discriminator": [
        199,
        61,
        169,
        235,
        49,
        162,
        162,
        70
      ]
    },
    {
      "name": "userStats",
      "discriminator": [
        176,
        223,
        136,
        27,
        122,
        79,
        32,
        227
      ]
    }
  ],
  "events": [
    {
      "name": "ammCurveChanged",
      "discriminator": [
        116,
        12,
        114,
        18,
        175,
        31,
        153,
        5
      ]
    },
    {
      "name": "deleteUserRecord",
      "discriminator": [
        71,
        111,
        190,
        118,
        7,
        3,
        132,
        222
      ]
    },
    {
      "name": "depositRecord",
      "discriminator": [
        180,
        241,
        218,
        207,
        102,
        135,
        44,
        134
      ]
    },
    {
      "name": "fundingPaymentRecord",
      "discriminator": [
        8,
        59,
        96,
        20,
        137,
        201,
        56,
        95
      ]
    },
    {
      "name": "fundingRateRecord",
      "discriminator": [
        68,
        3,
        255,
        26,
        133,
        91,
        147,
        254
      ]
    },
    {
      "name": "insuranceFundRecord",
      "discriminator": [
        56,
        222,
        215,
        235,
        78,
        197,
        99,
        146
      ]
    },
    {
      "name": "insuranceFundStakeRecord",
      "discriminator": [
        68,
        66,
        156,
        7,
        216,
        148,
        250,
        114
      ]
    },
    {
      "name": "lpBorrowLendDepositRecord",
      "discriminator": [
        242,
        181,
        11,
        56,
        243,
        61,
        79,
        210
      ]
    },
    {
      "name": "lpMintRedeemRecord",
      "discriminator": [
        53,
        178,
        142,
        73,
        78,
        91,
        91,
        8
      ]
    },
    {
      "name": "lpSettleRecord",
      "discriminator": [
        208,
        191,
        131,
        110,
        173,
        48,
        7,
        2
      ]
    },
    {
      "name": "lpSwapRecord",
      "discriminator": [
        159,
        62,
        130,
        196,
        96,
        79,
        176,
        254
      ]
    },
    {
      "name": "liquidationRecord",
      "discriminator": [
        127,
        17,
        0,
        108,
        182,
        13,
        231,
        53
      ]
    },
    {
      "name": "newUserRecord",
      "discriminator": [
        236,
        186,
        113,
        219,
        42,
        51,
        149,
        249
      ]
    },
    {
      "name": "orderActionRecord",
      "discriminator": [
        224,
        52,
        67,
        71,
        194,
        237,
        109,
        1
      ]
    },
    {
      "name": "orderRecord",
      "discriminator": [
        104,
        19,
        64,
        56,
        89,
        21,
        2,
        90
      ]
    },
    {
      "name": "perpMarketFeeSweepRecord",
      "discriminator": [
        55,
        107,
        227,
        104,
        179,
        8,
        121,
        33
      ]
    },
    {
      "name": "protocolFeeWithdrawRecord",
      "discriminator": [
        249,
        158,
        52,
        81,
        30,
        11,
        45,
        149
      ]
    },
    {
      "name": "protocolUserWithdrawRecordV0",
      "discriminator": [
        225,
        65,
        189,
        186,
        181,
        79,
        234,
        100
      ]
    },
    {
      "name": "revenueShareSettleRecord",
      "discriminator": [
        61,
        162,
        89,
        10,
        24,
        20,
        59,
        45
      ]
    },
    {
      "name": "settlePnlRecord",
      "discriminator": [
        57,
        68,
        105,
        26,
        119,
        198,
        213,
        89
      ]
    },
    {
      "name": "signedMsgOrderRecord",
      "discriminator": [
        211,
        197,
        25,
        18,
        142,
        86,
        113,
        27
      ]
    },
    {
      "name": "spotInterestRecord",
      "discriminator": [
        183,
        186,
        203,
        186,
        225,
        187,
        95,
        130
      ]
    },
    {
      "name": "spotMarketVaultDepositRecord",
      "discriminator": [
        178,
        217,
        23,
        188,
        127,
        190,
        32,
        73
      ]
    },
    {
      "name": "swapRecord",
      "discriminator": [
        162,
        187,
        123,
        194,
        138,
        56,
        250,
        241
      ]
    },
    {
      "name": "takerOriginCrossRecordV0",
      "discriminator": [
        124,
        217,
        157,
        153,
        11,
        104,
        35,
        145
      ]
    },
    {
      "name": "transferFeeAndPnlPoolRecord",
      "discriminator": [
        92,
        57,
        45,
        144,
        26,
        86,
        247,
        12
      ]
    },
    {
      "name": "signedMsgOrderParamsExport",
      "discriminator": [
        141,
        81,
        104,
        63,
        186,
        109,
        87,
        251
      ]
    }
  ],
  "errors": [
    {
      "code": 6000,
      "name": "invalidSpotMarketAuthority",
      "msg": "Invalid Spot Market Authority"
    },
    {
      "code": 6001,
      "name": "invalidInsuranceFundAuthority",
      "msg": "Clearing house not insurance fund authority"
    },
    {
      "code": 6002,
      "name": "insufficientDeposit",
      "msg": "Insufficient deposit"
    },
    {
      "code": 6003,
      "name": "insufficientCollateral",
      "msg": "Insufficient collateral"
    },
    {
      "code": 6004,
      "name": "sufficientCollateral",
      "msg": "Sufficient collateral"
    },
    {
      "code": 6005,
      "name": "maxNumberOfPositions",
      "msg": "Max number of positions taken"
    },
    {
      "code": 6006,
      "name": "adminControlsPricesDisabled",
      "msg": "Admin Controls Prices Disabled"
    },
    {
      "code": 6007,
      "name": "marketDelisted",
      "msg": "Market Delisted"
    },
    {
      "code": 6008,
      "name": "marketIndexAlreadyInitialized",
      "msg": "Market Index Already Initialized"
    },
    {
      "code": 6009,
      "name": "userAccountAndUserPositionsAccountMismatch",
      "msg": "User Account And User Positions Account Mismatch"
    },
    {
      "code": 6010,
      "name": "userHasNoPositionInMarket",
      "msg": "User Has No Position In Market"
    },
    {
      "code": 6011,
      "name": "invalidInitialPeg",
      "msg": "Invalid Initial Peg"
    },
    {
      "code": 6012,
      "name": "invalidRepegRedundant",
      "msg": "AMM repeg already configured with amt given"
    },
    {
      "code": 6013,
      "name": "invalidRepegDirection",
      "msg": "AMM repeg incorrect repeg direction"
    },
    {
      "code": 6014,
      "name": "invalidRepegProfitability",
      "msg": "AMM repeg out of bounds pnl"
    },
    {
      "code": 6015,
      "name": "slippageOutsideLimit",
      "msg": "Slippage Outside Limit Price"
    },
    {
      "code": 6016,
      "name": "orderSizeTooSmall",
      "msg": "Order Size Too Small"
    },
    {
      "code": 6017,
      "name": "invalidUpdateK",
      "msg": "Price change too large when updating K"
    },
    {
      "code": 6018,
      "name": "adminWithdrawTooLarge",
      "msg": "Admin tried to withdraw amount larger than fees collected"
    },
    {
      "code": 6019,
      "name": "mathError",
      "msg": "Math Error"
    },
    {
      "code": 6020,
      "name": "bnConversionError",
      "msg": "Conversion to u128/u64 failed with an overflow or underflow"
    },
    {
      "code": 6021,
      "name": "clockUnavailable",
      "msg": "Clock unavailable"
    },
    {
      "code": 6022,
      "name": "unableToLoadOracle",
      "msg": "Unable To Load Oracles"
    },
    {
      "code": 6023,
      "name": "priceBandsBreached",
      "msg": "Price Bands Breached"
    },
    {
      "code": 6024,
      "name": "exchangePaused",
      "msg": "Exchange is paused"
    },
    {
      "code": 6025,
      "name": "invalidWhitelistToken",
      "msg": "Invalid whitelist token"
    },
    {
      "code": 6026,
      "name": "whitelistTokenNotFound",
      "msg": "Whitelist token not found"
    },
    {
      "code": 6027,
      "name": "invalidDiscountToken",
      "msg": "Invalid discount token"
    },
    {
      "code": 6028,
      "name": "discountTokenNotFound",
      "msg": "Discount token not found"
    },
    {
      "code": 6029,
      "name": "referrerNotFound",
      "msg": "Referrer not found"
    },
    {
      "code": 6030,
      "name": "referrerStatsNotFound",
      "msg": "referrerNotFound"
    },
    {
      "code": 6031,
      "name": "referrerMustBeWritable",
      "msg": "referrerMustBeWritable"
    },
    {
      "code": 6032,
      "name": "referrerStatsMustBeWritable",
      "msg": "referrerMustBeWritable"
    },
    {
      "code": 6033,
      "name": "referrerAndReferrerStatsAuthorityUnequal",
      "msg": "referrerAndReferrerStatsAuthorityUnequal"
    },
    {
      "code": 6034,
      "name": "invalidReferrer",
      "msg": "invalidReferrer"
    },
    {
      "code": 6035,
      "name": "invalidOracle",
      "msg": "invalidOracle"
    },
    {
      "code": 6036,
      "name": "oracleNotFound",
      "msg": "oracleNotFound"
    },
    {
      "code": 6037,
      "name": "liquidationsBlockedByOracle",
      "msg": "Liquidations Blocked By Oracle"
    },
    {
      "code": 6038,
      "name": "maxDeposit",
      "msg": "Can not deposit more than max deposit"
    },
    {
      "code": 6039,
      "name": "cantDeleteUserWithCollateral",
      "msg": "Can not delete user that still has collateral"
    },
    {
      "code": 6040,
      "name": "invalidFundingProfitability",
      "msg": "AMM funding out of bounds pnl"
    },
    {
      "code": 6041,
      "name": "castingFailure",
      "msg": "Casting Failure"
    },
    {
      "code": 6042,
      "name": "invalidOrder",
      "msg": "invalidOrder"
    },
    {
      "code": 6043,
      "name": "invalidOrderMaxTs",
      "msg": "invalidOrderMaxTs"
    },
    {
      "code": 6044,
      "name": "invalidOrderMarketType",
      "msg": "invalidOrderMarketType"
    },
    {
      "code": 6045,
      "name": "invalidOrderForInitialMarginReq",
      "msg": "invalidOrderForInitialMarginReq"
    },
    {
      "code": 6046,
      "name": "invalidOrderNotRiskReducing",
      "msg": "invalidOrderNotRiskReducing"
    },
    {
      "code": 6047,
      "name": "invalidOrderSizeTooSmall",
      "msg": "invalidOrderSizeTooSmall"
    },
    {
      "code": 6048,
      "name": "invalidOrderNotStepSizeMultiple",
      "msg": "invalidOrderNotStepSizeMultiple"
    },
    {
      "code": 6049,
      "name": "invalidOrderBaseQuoteAsset",
      "msg": "invalidOrderBaseQuoteAsset"
    },
    {
      "code": 6050,
      "name": "invalidOrderIoc",
      "msg": "invalidOrderIoc"
    },
    {
      "code": 6051,
      "name": "invalidOrderPostOnly",
      "msg": "invalidOrderPostOnly"
    },
    {
      "code": 6052,
      "name": "invalidOrderIocPostOnly",
      "msg": "invalidOrderIocPostOnly"
    },
    {
      "code": 6053,
      "name": "invalidOrderTrigger",
      "msg": "invalidOrderTrigger"
    },
    {
      "code": 6054,
      "name": "invalidOrderAuction",
      "msg": "invalidOrderAuction"
    },
    {
      "code": 6055,
      "name": "invalidOrderOracleOffset",
      "msg": "invalidOrderOracleOffset"
    },
    {
      "code": 6056,
      "name": "invalidOrderMinOrderSize",
      "msg": "invalidOrderMinOrderSize"
    },
    {
      "code": 6057,
      "name": "placePostOnlyLimitFailure",
      "msg": "Failed to Place Post-Only Limit Order"
    },
    {
      "code": 6058,
      "name": "userHasNoOrder",
      "msg": "User has no order"
    },
    {
      "code": 6059,
      "name": "orderAmountTooSmall",
      "msg": "Order Amount Too Small"
    },
    {
      "code": 6060,
      "name": "maxNumberOfOrders",
      "msg": "Max number of orders taken"
    },
    {
      "code": 6061,
      "name": "orderDoesNotExist",
      "msg": "Order does not exist"
    },
    {
      "code": 6062,
      "name": "orderNotOpen",
      "msg": "Order not open"
    },
    {
      "code": 6063,
      "name": "fillOrderDidNotUpdateState",
      "msg": "fillOrderDidNotUpdateState"
    },
    {
      "code": 6064,
      "name": "reduceOnlyOrderIncreasedRisk",
      "msg": "Reduce only order increased risk"
    },
    {
      "code": 6065,
      "name": "unableToLoadAccountLoader",
      "msg": "Unable to load AccountLoader"
    },
    {
      "code": 6066,
      "name": "tradeSizeTooLarge",
      "msg": "Trade Size Too Large"
    },
    {
      "code": 6067,
      "name": "userCantReferThemselves",
      "msg": "User cant refer themselves"
    },
    {
      "code": 6068,
      "name": "didNotReceiveExpectedReferrer",
      "msg": "Did not receive expected referrer"
    },
    {
      "code": 6069,
      "name": "couldNotDeserializeReferrer",
      "msg": "Could not deserialize referrer"
    },
    {
      "code": 6070,
      "name": "couldNotDeserializeReferrerStats",
      "msg": "Could not deserialize referrer stats"
    },
    {
      "code": 6071,
      "name": "userOrderIdAlreadyInUse",
      "msg": "User Order Id Already In Use"
    },
    {
      "code": 6072,
      "name": "noPositionsLiquidatable",
      "msg": "No positions liquidatable"
    },
    {
      "code": 6073,
      "name": "invalidMarginRatio",
      "msg": "Invalid Margin Ratio"
    },
    {
      "code": 6074,
      "name": "cantCancelPostOnlyOrder",
      "msg": "Cant Cancel Post Only Order"
    },
    {
      "code": 6075,
      "name": "invalidOracleOffset",
      "msg": "invalidOracleOffset"
    },
    {
      "code": 6076,
      "name": "cantExpireOrders",
      "msg": "cantExpireOrders"
    },
    {
      "code": 6077,
      "name": "couldNotLoadMarketData",
      "msg": "couldNotLoadMarketData"
    },
    {
      "code": 6078,
      "name": "perpMarketNotFound",
      "msg": "perpMarketNotFound"
    },
    {
      "code": 6079,
      "name": "invalidMarketAccount",
      "msg": "invalidMarketAccount"
    },
    {
      "code": 6080,
      "name": "unableToLoadPerpMarketAccount",
      "msg": "unableToLoadMarketAccount"
    },
    {
      "code": 6081,
      "name": "marketWrongMutability",
      "msg": "marketWrongMutability"
    },
    {
      "code": 6082,
      "name": "unableToCastUnixTime",
      "msg": "unableToCastUnixTime"
    },
    {
      "code": 6083,
      "name": "couldNotFindSpotPosition",
      "msg": "couldNotFindSpotPosition"
    },
    {
      "code": 6084,
      "name": "noSpotPositionAvailable",
      "msg": "noSpotPositionAvailable"
    },
    {
      "code": 6085,
      "name": "invalidSpotMarketInitialization",
      "msg": "invalidSpotMarketInitialization"
    },
    {
      "code": 6086,
      "name": "couldNotLoadSpotMarketData",
      "msg": "couldNotLoadSpotMarketData"
    },
    {
      "code": 6087,
      "name": "spotMarketNotFound",
      "msg": "spotMarketNotFound"
    },
    {
      "code": 6088,
      "name": "invalidSpotMarketAccount",
      "msg": "invalidSpotMarketAccount"
    },
    {
      "code": 6089,
      "name": "unableToLoadSpotMarketAccount",
      "msg": "unableToLoadSpotMarketAccount"
    },
    {
      "code": 6090,
      "name": "spotMarketWrongMutability",
      "msg": "spotMarketWrongMutability"
    },
    {
      "code": 6091,
      "name": "spotMarketInterestNotUpToDate",
      "msg": "spotInterestNotUpToDate"
    },
    {
      "code": 6092,
      "name": "spotMarketInsufficientDeposits",
      "msg": "spotMarketInsufficientDeposits"
    },
    {
      "code": 6093,
      "name": "userMustSettleTheirOwnPositiveUnsettledPnl",
      "msg": "userMustSettleTheirOwnPositiveUnsettledPnl"
    },
    {
      "code": 6094,
      "name": "cantUpdateSpotBalanceType",
      "msg": "cantUpdateSpotBalanceType"
    },
    {
      "code": 6095,
      "name": "insufficientCollateralForSettlingPnl",
      "msg": "insufficientCollateralForSettlingPnl"
    },
    {
      "code": 6096,
      "name": "ammNotUpdatedInSameSlot",
      "msg": "ammNotUpdatedInSameSlot"
    },
    {
      "code": 6097,
      "name": "auctionNotComplete",
      "msg": "auctionNotComplete"
    },
    {
      "code": 6098,
      "name": "makerNotFound",
      "msg": "makerNotFound"
    },
    {
      "code": 6099,
      "name": "makerStatsNotFound",
      "msg": "makerNotFound"
    },
    {
      "code": 6100,
      "name": "makerMustBeWritable",
      "msg": "makerMustBeWritable"
    },
    {
      "code": 6101,
      "name": "makerStatsMustBeWritable",
      "msg": "makerMustBeWritable"
    },
    {
      "code": 6102,
      "name": "makerOrderNotFound",
      "msg": "makerOrderNotFound"
    },
    {
      "code": 6103,
      "name": "couldNotDeserializeMaker",
      "msg": "couldNotDeserializeMaker"
    },
    {
      "code": 6104,
      "name": "couldNotDeserializeMakerStats",
      "msg": "couldNotDeserializeMaker"
    },
    {
      "code": 6105,
      "name": "auctionPriceDoesNotSatisfyMaker",
      "msg": "auctionPriceDoesNotSatisfyMaker"
    },
    {
      "code": 6106,
      "name": "makerCantFulfillOwnOrder",
      "msg": "makerCantFulfillOwnOrder"
    },
    {
      "code": 6107,
      "name": "makerOrderMustBePostOnly",
      "msg": "makerOrderMustBePostOnly"
    },
    {
      "code": 6108,
      "name": "cantMatchTwoPostOnlys",
      "msg": "cantMatchTwoPostOnlys"
    },
    {
      "code": 6109,
      "name": "orderBreachesOraclePriceLimits",
      "msg": "orderBreachesOraclePriceLimits"
    },
    {
      "code": 6110,
      "name": "orderMustBeTriggeredFirst",
      "msg": "orderMustBeTriggeredFirst"
    },
    {
      "code": 6111,
      "name": "orderNotTriggerable",
      "msg": "orderNotTriggerable"
    },
    {
      "code": 6112,
      "name": "orderDidNotSatisfyTriggerCondition",
      "msg": "orderDidNotSatisfyTriggerCondition"
    },
    {
      "code": 6113,
      "name": "positionAlreadyBeingLiquidated",
      "msg": "positionAlreadyBeingLiquidated"
    },
    {
      "code": 6114,
      "name": "positionDoesntHaveOpenPositionOrOrders",
      "msg": "positionDoesntHaveOpenPositionOrOrders"
    },
    {
      "code": 6115,
      "name": "allOrdersAreAlreadyLiquidations",
      "msg": "allOrdersAreAlreadyLiquidations"
    },
    {
      "code": 6116,
      "name": "cantCancelLiquidationOrder",
      "msg": "cantCancelLiquidationOrder"
    },
    {
      "code": 6117,
      "name": "userIsBeingLiquidated",
      "msg": "userIsBeingLiquidated"
    },
    {
      "code": 6118,
      "name": "liquidationsOngoing",
      "msg": "liquidationsOngoing"
    },
    {
      "code": 6119,
      "name": "wrongSpotBalanceType",
      "msg": "wrongSpotBalanceType"
    },
    {
      "code": 6120,
      "name": "userCantLiquidateThemself",
      "msg": "userCantLiquidateThemself"
    },
    {
      "code": 6121,
      "name": "invalidPerpPositionToLiquidate",
      "msg": "invalidPerpPositionToLiquidate"
    },
    {
      "code": 6122,
      "name": "invalidBaseAssetAmountForLiquidatePerp",
      "msg": "invalidBaseAssetAmountForLiquidatePerp"
    },
    {
      "code": 6123,
      "name": "invalidPositionLastFundingRate",
      "msg": "invalidPositionLastFundingRate"
    },
    {
      "code": 6124,
      "name": "invalidPositionDelta",
      "msg": "invalidPositionDelta"
    },
    {
      "code": 6125,
      "name": "userBankrupt",
      "msg": "userBankrupt"
    },
    {
      "code": 6126,
      "name": "userNotBankrupt",
      "msg": "userNotBankrupt"
    },
    {
      "code": 6127,
      "name": "userHasInvalidBorrow",
      "msg": "userHasInvalidBorrow"
    },
    {
      "code": 6128,
      "name": "dailyWithdrawLimit",
      "msg": "dailyWithdrawLimit"
    },
    {
      "code": 6129,
      "name": "defaultError",
      "msg": "defaultError"
    },
    {
      "code": 6130,
      "name": "insufficientLpTokens",
      "msg": "Insufficient LP tokens"
    },
    {
      "code": 6131,
      "name": "cantLpWithPerpPosition",
      "msg": "Cant LP with a market position"
    },
    {
      "code": 6132,
      "name": "unableToBurnLpTokens",
      "msg": "Unable to burn LP tokens"
    },
    {
      "code": 6133,
      "name": "tryingToRemoveLiquidityTooFast",
      "msg": "Trying to remove liqudity too fast after adding it"
    },
    {
      "code": 6134,
      "name": "invalidSpotMarketVault",
      "msg": "Invalid Spot Market Vault"
    },
    {
      "code": 6135,
      "name": "invalidSpotMarketState",
      "msg": "Invalid Spot Market State"
    },
    {
      "code": 6136,
      "name": "invalidSerumProgram",
      "msg": "invalidSerumProgram"
    },
    {
      "code": 6137,
      "name": "invalidSerumMarket",
      "msg": "invalidSerumMarket"
    },
    {
      "code": 6138,
      "name": "invalidSerumBids",
      "msg": "invalidSerumBids"
    },
    {
      "code": 6139,
      "name": "invalidSerumAsks",
      "msg": "invalidSerumAsks"
    },
    {
      "code": 6140,
      "name": "invalidSerumOpenOrders",
      "msg": "invalidSerumOpenOrders"
    },
    {
      "code": 6141,
      "name": "failedSerumCpi",
      "msg": "failedSerumCpi"
    },
    {
      "code": 6142,
      "name": "failedToFillOnExternalMarket",
      "msg": "failedToFillOnExternalMarket"
    },
    {
      "code": 6143,
      "name": "invalidFulfillmentConfig",
      "msg": "invalidFulfillmentConfig"
    },
    {
      "code": 6144,
      "name": "invalidFeeStructure",
      "msg": "invalidFeeStructure"
    },
    {
      "code": 6145,
      "name": "insufficientIfShares",
      "msg": "Insufficient IF shares"
    },
    {
      "code": 6146,
      "name": "marketActionPaused",
      "msg": "the Market has paused this action"
    },
    {
      "code": 6147,
      "name": "marketPlaceOrderPaused",
      "msg": "the Market status doesnt allow placing orders"
    },
    {
      "code": 6148,
      "name": "marketFillOrderPaused",
      "msg": "the Market status doesnt allow filling orders"
    },
    {
      "code": 6149,
      "name": "marketWithdrawPaused",
      "msg": "the Market status doesnt allow withdraws"
    },
    {
      "code": 6150,
      "name": "protectedAssetTierViolation",
      "msg": "Action violates the Protected Asset Tier rules"
    },
    {
      "code": 6151,
      "name": "isolatedAssetTierViolation",
      "msg": "Action violates the Isolated Asset Tier rules"
    },
    {
      "code": 6152,
      "name": "userCantBeDeleted",
      "msg": "User Cant Be Deleted"
    },
    {
      "code": 6153,
      "name": "reduceOnlyWithdrawIncreasedRisk",
      "msg": "Reduce Only Withdraw Increased Risk"
    },
    {
      "code": 6154,
      "name": "maxOpenInterest",
      "msg": "Max Open Interest"
    },
    {
      "code": 6155,
      "name": "cantResolvePerpBankruptcy",
      "msg": "Cant Resolve Perp Bankruptcy"
    },
    {
      "code": 6156,
      "name": "liquidationDoesntSatisfyLimitPrice",
      "msg": "Liquidation Doesnt Satisfy Limit Price"
    },
    {
      "code": 6157,
      "name": "marginTradingDisabled",
      "msg": "Margin Trading Disabled"
    },
    {
      "code": 6158,
      "name": "invalidMarketStatusToSettlePnl",
      "msg": "Invalid Market Status to Settle Perp Pnl"
    },
    {
      "code": 6159,
      "name": "perpMarketNotInSettlement",
      "msg": "perpMarketNotInSettlement"
    },
    {
      "code": 6160,
      "name": "perpMarketNotInReduceOnly",
      "msg": "perpMarketNotInReduceOnly"
    },
    {
      "code": 6161,
      "name": "perpMarketSettlementBufferNotReached",
      "msg": "perpMarketSettlementBufferNotReached"
    },
    {
      "code": 6162,
      "name": "perpMarketSettlementUserHasOpenOrders",
      "msg": "perpMarketSettlementUserHasOpenOrders"
    },
    {
      "code": 6163,
      "name": "perpMarketSettlementUserHasActiveLp",
      "msg": "perpMarketSettlementUserHasActiveLp"
    },
    {
      "code": 6164,
      "name": "unableToSettleExpiredUserPosition",
      "msg": "unableToSettleExpiredUserPosition"
    },
    {
      "code": 6165,
      "name": "unequalMarketIndexForSpotTransfer",
      "msg": "unequalMarketIndexForSpotTransfer"
    },
    {
      "code": 6166,
      "name": "invalidPerpPositionDetected",
      "msg": "invalidPerpPositionDetected"
    },
    {
      "code": 6167,
      "name": "invalidSpotPositionDetected",
      "msg": "invalidSpotPositionDetected"
    },
    {
      "code": 6168,
      "name": "invalidAmmDetected",
      "msg": "invalidAmmDetected"
    },
    {
      "code": 6169,
      "name": "invalidAmmForFillDetected",
      "msg": "invalidAmmForFillDetected"
    },
    {
      "code": 6170,
      "name": "invalidAmmLimitPriceOverride",
      "msg": "invalidAmmLimitPriceOverride"
    },
    {
      "code": 6171,
      "name": "invalidOrderFillPrice",
      "msg": "invalidOrderFillPrice"
    },
    {
      "code": 6172,
      "name": "spotMarketBalanceInvariantViolated",
      "msg": "spotMarketBalanceInvariantViolated"
    },
    {
      "code": 6173,
      "name": "spotMarketVaultInvariantViolated",
      "msg": "spotMarketVaultInvariantViolated"
    },
    {
      "code": 6174,
      "name": "invalidPda",
      "msg": "invalidPda"
    },
    {
      "code": 6175,
      "name": "invalidPdaSigner",
      "msg": "invalidPdaSigner"
    },
    {
      "code": 6176,
      "name": "revenueSettingsCannotSettleToIf",
      "msg": "revenueSettingsCannotSettleToIf"
    },
    {
      "code": 6177,
      "name": "noRevenueToSettleToIf",
      "msg": "noRevenueToSettleToIf"
    },
    {
      "code": 6178,
      "name": "noAmmPerpPnlDeficit",
      "msg": "noAmmPerpPnlDeficit"
    },
    {
      "code": 6179,
      "name": "sufficientPerpPnlPool",
      "msg": "sufficientPerpPnlPool"
    },
    {
      "code": 6180,
      "name": "insufficientPerpPnlPool",
      "msg": "insufficientPerpPnlPool"
    },
    {
      "code": 6181,
      "name": "perpPnlDeficitBelowThreshold",
      "msg": "perpPnlDeficitBelowThreshold"
    },
    {
      "code": 6182,
      "name": "maxRevenueWithdrawPerPeriodReached",
      "msg": "maxRevenueWithdrawPerPeriodReached"
    },
    {
      "code": 6183,
      "name": "maxIfWithdrawReached",
      "msg": "invalidSpotPositionDetected"
    },
    {
      "code": 6184,
      "name": "noIfWithdrawAvailable",
      "msg": "noIfWithdrawAvailable"
    },
    {
      "code": 6185,
      "name": "invalidIfUnstake",
      "msg": "invalidIfUnstake"
    },
    {
      "code": 6186,
      "name": "invalidIfUnstakeSize",
      "msg": "invalidIfUnstakeSize"
    },
    {
      "code": 6187,
      "name": "invalidIfUnstakeCancel",
      "msg": "invalidIfUnstakeCancel"
    },
    {
      "code": 6188,
      "name": "invalidIfForNewStakes",
      "msg": "invalidIfForNewStakes"
    },
    {
      "code": 6189,
      "name": "invalidIfRebase",
      "msg": "invalidIfRebase"
    },
    {
      "code": 6190,
      "name": "invalidInsuranceUnstakeSize",
      "msg": "invalidInsuranceUnstakeSize"
    },
    {
      "code": 6191,
      "name": "invalidOrderLimitPrice",
      "msg": "invalidOrderLimitPrice"
    },
    {
      "code": 6192,
      "name": "invalidIfDetected",
      "msg": "invalidIfDetected"
    },
    {
      "code": 6193,
      "name": "invalidAmmMaxSpreadDetected",
      "msg": "invalidAmmMaxSpreadDetected"
    },
    {
      "code": 6194,
      "name": "invalidConcentrationCoef",
      "msg": "invalidConcentrationCoef"
    },
    {
      "code": 6195,
      "name": "invalidSrmVault",
      "msg": "invalidSrmVault"
    },
    {
      "code": 6196,
      "name": "invalidVaultOwner",
      "msg": "invalidVaultOwner"
    },
    {
      "code": 6197,
      "name": "invalidMarketStatusForFills",
      "msg": "invalidMarketStatusForFills"
    },
    {
      "code": 6198,
      "name": "ifWithdrawRequestInProgress",
      "msg": "ifWithdrawRequestInProgress"
    },
    {
      "code": 6199,
      "name": "noIfWithdrawRequestInProgress",
      "msg": "noIfWithdrawRequestInProgress"
    },
    {
      "code": 6200,
      "name": "ifWithdrawRequestTooSmall",
      "msg": "ifWithdrawRequestTooSmall"
    },
    {
      "code": 6201,
      "name": "incorrectSpotMarketAccountPassed",
      "msg": "incorrectSpotMarketAccountPassed"
    },
    {
      "code": 6202,
      "name": "blockchainClockInconsistency",
      "msg": "blockchainClockInconsistency"
    },
    {
      "code": 6203,
      "name": "invalidIfSharesDetected",
      "msg": "invalidIfSharesDetected"
    },
    {
      "code": 6204,
      "name": "newLpSizeTooSmall",
      "msg": "newLpSizeTooSmall"
    },
    {
      "code": 6205,
      "name": "marketStatusInvalidForNewLp",
      "msg": "marketStatusInvalidForNewLp"
    },
    {
      "code": 6206,
      "name": "invalidMarkTwapUpdateDetected",
      "msg": "invalidMarkTwapUpdateDetected"
    },
    {
      "code": 6207,
      "name": "marketSettlementAttemptOnActiveMarket",
      "msg": "marketSettlementAttemptOnActiveMarket"
    },
    {
      "code": 6208,
      "name": "marketSettlementRequiresSettledLp",
      "msg": "marketSettlementRequiresSettledLp"
    },
    {
      "code": 6209,
      "name": "marketSettlementAttemptTooEarly",
      "msg": "marketSettlementAttemptTooEarly"
    },
    {
      "code": 6210,
      "name": "marketSettlementTargetPriceInvalid",
      "msg": "marketSettlementTargetPriceInvalid"
    },
    {
      "code": 6211,
      "name": "unsupportedSpotMarket",
      "msg": "unsupportedSpotMarket"
    },
    {
      "code": 6212,
      "name": "spotOrdersDisabled",
      "msg": "spotOrdersDisabled"
    },
    {
      "code": 6213,
      "name": "marketBeingInitialized",
      "msg": "Market Being Initialized"
    },
    {
      "code": 6214,
      "name": "invalidUserSubAccountId",
      "msg": "Invalid Sub Account Id"
    },
    {
      "code": 6215,
      "name": "invalidTriggerOrderCondition",
      "msg": "Invalid Trigger Order Condition"
    },
    {
      "code": 6216,
      "name": "invalidSpotPosition",
      "msg": "Invalid Spot Position"
    },
    {
      "code": 6217,
      "name": "cantTransferBetweenSameUserAccount",
      "msg": "Cant transfer between same user account"
    },
    {
      "code": 6218,
      "name": "invalidPerpPosition",
      "msg": "Invalid Perp Position"
    },
    {
      "code": 6219,
      "name": "unableToGetLimitPrice",
      "msg": "Unable To Get Limit Price"
    },
    {
      "code": 6220,
      "name": "invalidLiquidation",
      "msg": "Invalid Liquidation"
    },
    {
      "code": 6221,
      "name": "spotFulfillmentConfigDisabled",
      "msg": "Spot Fulfillment Config Disabled"
    },
    {
      "code": 6222,
      "name": "invalidMaker",
      "msg": "Invalid Maker"
    },
    {
      "code": 6223,
      "name": "failedUnwrap",
      "msg": "Failed Unwrap"
    },
    {
      "code": 6224,
      "name": "maxNumberOfUsers",
      "msg": "Max Number Of Users"
    },
    {
      "code": 6225,
      "name": "invalidOracleForSettlePnl",
      "msg": "invalidOracleForSettlePnl"
    },
    {
      "code": 6226,
      "name": "marginOrdersOpen",
      "msg": "marginOrdersOpen"
    },
    {
      "code": 6227,
      "name": "tierViolationLiquidatingPerpPnl",
      "msg": "tierViolationLiquidatingPerpPnl"
    },
    {
      "code": 6228,
      "name": "couldNotLoadUserData",
      "msg": "couldNotLoadUserData"
    },
    {
      "code": 6229,
      "name": "userWrongMutability",
      "msg": "userWrongMutability"
    },
    {
      "code": 6230,
      "name": "invalidUserAccount",
      "msg": "invalidUserAccount"
    },
    {
      "code": 6231,
      "name": "couldNotLoadUserStatsData",
      "msg": "couldNotLoadUserData"
    },
    {
      "code": 6232,
      "name": "userStatsWrongMutability",
      "msg": "userWrongMutability"
    },
    {
      "code": 6233,
      "name": "invalidUserStatsAccount",
      "msg": "invalidUserAccount"
    },
    {
      "code": 6234,
      "name": "userNotFound",
      "msg": "userNotFound"
    },
    {
      "code": 6235,
      "name": "unableToLoadUserAccount",
      "msg": "unableToLoadUserAccount"
    },
    {
      "code": 6236,
      "name": "userStatsNotFound",
      "msg": "userStatsNotFound"
    },
    {
      "code": 6237,
      "name": "unableToLoadUserStatsAccount",
      "msg": "unableToLoadUserStatsAccount"
    },
    {
      "code": 6238,
      "name": "userNotInactive",
      "msg": "User Not Inactive"
    },
    {
      "code": 6239,
      "name": "revertFill",
      "msg": "revertFill"
    },
    {
      "code": 6240,
      "name": "invalidMarketAccountforDeletion",
      "msg": "Invalid MarketAccount for Deletion"
    },
    {
      "code": 6241,
      "name": "invalidSpotFulfillmentParams",
      "msg": "Invalid Spot Fulfillment Params"
    },
    {
      "code": 6242,
      "name": "failedToGetMint",
      "msg": "Failed to Get Mint"
    },
    {
      "code": 6243,
      "name": "failedPhoenixCpi",
      "msg": "failedPhoenixCpi"
    },
    {
      "code": 6244,
      "name": "failedToDeserializePhoenixMarket",
      "msg": "failedToDeserializePhoenixMarket"
    },
    {
      "code": 6245,
      "name": "invalidPricePrecision",
      "msg": "invalidPricePrecision"
    },
    {
      "code": 6246,
      "name": "invalidPhoenixProgram",
      "msg": "invalidPhoenixProgram"
    },
    {
      "code": 6247,
      "name": "invalidPhoenixMarket",
      "msg": "invalidPhoenixMarket"
    },
    {
      "code": 6248,
      "name": "invalidSwap",
      "msg": "invalidSwap"
    },
    {
      "code": 6249,
      "name": "swapLimitPriceBreached",
      "msg": "swapLimitPriceBreached"
    },
    {
      "code": 6250,
      "name": "spotMarketReduceOnly",
      "msg": "spotMarketReduceOnly"
    },
    {
      "code": 6251,
      "name": "fundingWasNotUpdated",
      "msg": "fundingWasNotUpdated"
    },
    {
      "code": 6252,
      "name": "impossibleFill",
      "msg": "impossibleFill"
    },
    {
      "code": 6253,
      "name": "cantUpdatePerpBidAskTwap",
      "msg": "cantUpdatePerpBidAskTwap"
    },
    {
      "code": 6254,
      "name": "userReduceOnly",
      "msg": "userReduceOnly"
    },
    {
      "code": 6255,
      "name": "invalidMarginCalculation",
      "msg": "invalidMarginCalculation"
    },
    {
      "code": 6256,
      "name": "cantPayUserInitFee",
      "msg": "cantPayUserInitFee"
    },
    {
      "code": 6257,
      "name": "cantReclaimRent",
      "msg": "cantReclaimRent"
    },
    {
      "code": 6258,
      "name": "insuranceFundOperationPaused",
      "msg": "insuranceFundOperationPaused"
    },
    {
      "code": 6259,
      "name": "noUnsettledPnl",
      "msg": "noUnsettledPnl"
    },
    {
      "code": 6260,
      "name": "pnlPoolCantSettleUser",
      "msg": "pnlPoolCantSettleUser"
    },
    {
      "code": 6261,
      "name": "oracleNonPositive",
      "msg": "oracleInvalid"
    },
    {
      "code": 6262,
      "name": "oracleTooVolatile",
      "msg": "oracleTooVolatile"
    },
    {
      "code": 6263,
      "name": "oracleTooUncertain",
      "msg": "oracleTooUncertain"
    },
    {
      "code": 6264,
      "name": "oracleStaleForMargin",
      "msg": "oracleStaleForMargin"
    },
    {
      "code": 6265,
      "name": "oracleInsufficientDataPoints",
      "msg": "oracleInsufficientDataPoints"
    },
    {
      "code": 6266,
      "name": "oracleStaleForAmm",
      "msg": "oracleStaleForAmm"
    },
    {
      "code": 6267,
      "name": "unableToParsePullOracleMessage",
      "msg": "Unable to parse pull oracle message"
    },
    {
      "code": 6268,
      "name": "maxBorrows",
      "msg": "Can not borow more than max borrows"
    },
    {
      "code": 6269,
      "name": "oracleUpdatesNotMonotonic",
      "msg": "Updates must be monotonically increasing"
    },
    {
      "code": 6270,
      "name": "oraclePriceFeedMessageMismatch",
      "msg": "Trying to update price feed with the wrong feed id"
    },
    {
      "code": 6271,
      "name": "oracleUnsupportedMessageType",
      "msg": "The message in the update must be a PriceFeedMessage"
    },
    {
      "code": 6272,
      "name": "oracleDeserializeMessageFailed",
      "msg": "Could not deserialize the message in the update"
    },
    {
      "code": 6273,
      "name": "oracleWrongGuardianSetOwner",
      "msg": "Wrong guardian set owner in update price atomic"
    },
    {
      "code": 6274,
      "name": "oracleWrongWriteAuthority",
      "msg": "Oracle post update atomic price feed account must be velocity program"
    },
    {
      "code": 6275,
      "name": "oracleWrongVaaOwner",
      "msg": "Oracle vaa owner must be wormhole program"
    },
    {
      "code": 6276,
      "name": "oracleTooManyPriceAccountUpdates",
      "msg": "Multi updates must have 2 or fewer accounts passed in remaining accounts"
    },
    {
      "code": 6277,
      "name": "oracleMismatchedVaaAndPriceUpdates",
      "msg": "Don't have the same remaining accounts number and pyth updates left"
    },
    {
      "code": 6278,
      "name": "oracleBadRemainingAccountPublicKey",
      "msg": "Remaining account passed does not match oracle update derived pda"
    },
    {
      "code": 6279,
      "name": "failedOpenbookV2cpi",
      "msg": "failedOpenbookV2cpi"
    },
    {
      "code": 6280,
      "name": "invalidOpenbookV2Program",
      "msg": "invalidOpenbookV2Program"
    },
    {
      "code": 6281,
      "name": "invalidOpenbookV2Market",
      "msg": "invalidOpenbookV2Market"
    },
    {
      "code": 6282,
      "name": "nonZeroTransferFee",
      "msg": "Non zero transfer fee"
    },
    {
      "code": 6283,
      "name": "liquidationOrderFailedToFill",
      "msg": "Liquidation order failed to fill"
    },
    {
      "code": 6284,
      "name": "depreciatedPredictionMarketOrder",
      "msg": "deprecated"
    },
    {
      "code": 6285,
      "name": "invalidVerificationIxIndex",
      "msg": "Ed25519 Ix must be before place and make SignedMsg order ix"
    },
    {
      "code": 6286,
      "name": "sigVerificationFailed",
      "msg": "SignedMsg message verificaiton failed"
    },
    {
      "code": 6287,
      "name": "mismatchedSignedMsgOrderParamsMarketIndex",
      "msg": "Market index mismatched b/w taker and maker SignedMsg order params"
    },
    {
      "code": 6288,
      "name": "invalidSignedMsgOrderParam",
      "msg": "Invalid SignedMsg order param"
    },
    {
      "code": 6289,
      "name": "placeAndTakeOrderSuccessConditionFailed",
      "msg": "Place and take order success condition failed"
    },
    {
      "code": 6290,
      "name": "deprecatedHighLeverageModeConfig",
      "msg": "deprecated"
    },
    {
      "code": 6291,
      "name": "invalidRfqUserAccount",
      "msg": "Invalid RFQ User Account"
    },
    {
      "code": 6292,
      "name": "rfqUserAccountWrongMutability",
      "msg": "RFQUserAccount should be mutable"
    },
    {
      "code": 6293,
      "name": "rfqUserAccountFull",
      "msg": "RFQUserAccount has too many active RFQs"
    },
    {
      "code": 6294,
      "name": "rfqOrderNotFilled",
      "msg": "RFQ order not filled as expected"
    },
    {
      "code": 6295,
      "name": "invalidRfqOrder",
      "msg": "RFQ orders must be jit makers"
    },
    {
      "code": 6296,
      "name": "invalidRfqMatch",
      "msg": "RFQ matches must be valid"
    },
    {
      "code": 6297,
      "name": "invalidSignedMsgUserAccount",
      "msg": "Invalid SignedMsg user account"
    },
    {
      "code": 6298,
      "name": "signedMsgUserAccountWrongMutability",
      "msg": "SignedMsg account wrong mutability"
    },
    {
      "code": 6299,
      "name": "signedMsgUserOrdersAccountFull",
      "msg": "SignedMsgUserAccount has too many active orders"
    },
    {
      "code": 6300,
      "name": "signedMsgOrderDoesNotExist",
      "msg": "Order with SignedMsg uuid does not exist"
    },
    {
      "code": 6301,
      "name": "invalidSignedMsgOrderId",
      "msg": "SignedMsg order id cannot be 0s"
    },
    {
      "code": 6302,
      "name": "invalidPoolId",
      "msg": "Invalid pool id"
    },
    {
      "code": 6303,
      "name": "invalidProtectedMakerModeConfig",
      "msg": "Invalid Protected Maker Mode Config"
    },
    {
      "code": 6304,
      "name": "invalidPythLazerStorageOwner",
      "msg": "Invalid pyth lazer storage owner"
    },
    {
      "code": 6305,
      "name": "unverifiedPythLazerMessage",
      "msg": "Verification of pyth lazer message failed"
    },
    {
      "code": 6306,
      "name": "invalidPythLazerMessage",
      "msg": "Invalid pyth lazer message"
    },
    {
      "code": 6307,
      "name": "pythLazerMessagePriceFeedMismatch",
      "msg": "Pyth lazer message does not correspond to correct fed id"
    },
    {
      "code": 6308,
      "name": "invalidLiquidateSpotWithSwap",
      "msg": "invalidLiquidateSpotWithSwap"
    },
    {
      "code": 6309,
      "name": "signedMsgUserContextUserMismatch",
      "msg": "User in SignedMsg message does not match user in ix context"
    },
    {
      "code": 6310,
      "name": "deprecated1",
      "msg": "deprecated"
    },
    {
      "code": 6311,
      "name": "deprecated2",
      "msg": "deprecated"
    },
    {
      "code": 6312,
      "name": "invalidTransferPerpPosition",
      "msg": "Invalid Transfer Perp Position"
    },
    {
      "code": 6313,
      "name": "invalidSignedMsgUserOrdersResize",
      "msg": "Invalid SignedMsgUserOrders resize"
    },
    {
      "code": 6314,
      "name": "deprecatedCouldNotDeserializeHighLeverageModeConfig",
      "msg": "deprecated"
    },
    {
      "code": 6315,
      "name": "invalidIfRebalanceConfig",
      "msg": "Invalid If Rebalance Config"
    },
    {
      "code": 6316,
      "name": "invalidIfRebalanceSwap",
      "msg": "Invalid If Rebalance Swap"
    },
    {
      "code": 6317,
      "name": "invalidRevenueShareResize",
      "msg": "Invalid RevenueShare resize"
    },
    {
      "code": 6318,
      "name": "builderRevoked",
      "msg": "Builder has been revoked"
    },
    {
      "code": 6319,
      "name": "invalidBuilderFee",
      "msg": "Builder fee is greater than max fee bps"
    },
    {
      "code": 6320,
      "name": "revenueShareEscrowAuthorityMismatch",
      "msg": "RevenueShareEscrow authority mismatch"
    },
    {
      "code": 6321,
      "name": "revenueShareEscrowOrdersAccountFull",
      "msg": "RevenueShareEscrow has too many active orders"
    },
    {
      "code": 6322,
      "name": "invalidRevenueShareAccount",
      "msg": "Invalid RevenueShareAccount"
    },
    {
      "code": 6323,
      "name": "cannotRevokeBuilderWithOpenOrders",
      "msg": "Cannot revoke builder with open orders"
    },
    {
      "code": 6324,
      "name": "unableToLoadRevenueShareAccount",
      "msg": "Unable to load builder account"
    },
    {
      "code": 6325,
      "name": "invalidConstituent",
      "msg": "Invalid Constituent"
    },
    {
      "code": 6326,
      "name": "invalidAmmConstituentMappingArgument",
      "msg": "Invalid Amm Constituent Mapping argument"
    },
    {
      "code": 6327,
      "name": "constituentNotFound",
      "msg": "Constituent not found"
    },
    {
      "code": 6328,
      "name": "constituentCouldNotLoad",
      "msg": "Constituent could not load"
    },
    {
      "code": 6329,
      "name": "constituentWrongMutability",
      "msg": "Constituent wrong mutability"
    },
    {
      "code": 6330,
      "name": "wrongNumberOfConstituents",
      "msg": "Wrong number of constituents passed to instruction"
    },
    {
      "code": 6331,
      "name": "insufficientConstituentTokenBalance",
      "msg": "Insufficient constituent token balance"
    },
    {
      "code": 6332,
      "name": "ammCacheStale",
      "msg": "Amm Cache data too stale"
    },
    {
      "code": 6333,
      "name": "lpPoolAumDelayed",
      "msg": "LP Pool AUM not updated recently"
    },
    {
      "code": 6334,
      "name": "constituentOracleStale",
      "msg": "Constituent oracle is stale"
    },
    {
      "code": 6335,
      "name": "lpInvariantFailed",
      "msg": "LP Invariant failed"
    },
    {
      "code": 6336,
      "name": "invalidConstituentDerivativeWeights",
      "msg": "Invalid constituent derivative weights"
    },
    {
      "code": 6337,
      "name": "maxDlpAumBreached",
      "msg": "Max DLP AUM Breached"
    },
    {
      "code": 6338,
      "name": "settleLpPoolDisabled",
      "msg": "Settle Lp Pool Disabled"
    },
    {
      "code": 6339,
      "name": "mintRedeemLpPoolDisabled",
      "msg": "Mint/Redeem Lp Pool Disabled"
    },
    {
      "code": 6340,
      "name": "lpPoolSettleInvariantBreached",
      "msg": "Settlement amount exceeded"
    },
    {
      "code": 6341,
      "name": "invalidConstituentOperation",
      "msg": "Invalid constituent operation"
    },
    {
      "code": 6342,
      "name": "unauthorized",
      "msg": "Unauthorized for operation"
    },
    {
      "code": 6343,
      "name": "invalidLpPoolId",
      "msg": "Invalid Lp Pool Id for Operation"
    },
    {
      "code": 6344,
      "name": "marketIndexNotFoundAmmCache",
      "msg": "marketIndexNotFoundAmmCache"
    },
    {
      "code": 6345,
      "name": "invalidIsolatedPerpMarket",
      "msg": "Invalid Isolated Perp Market"
    },
    {
      "code": 6346,
      "name": "invalidOrderScaleOrderCount",
      "msg": "Invalid scale order count - must be between 2 and 10"
    },
    {
      "code": 6347,
      "name": "invalidOrderScalePriceRange",
      "msg": "Invalid scale order price range"
    },
    {
      "code": 6348,
      "name": "invalidPerpMarketConfig",
      "msg": "Invalid perp market config"
    },
    {
      "code": 6349,
      "name": "invalidInsuranceFundWithdrawalRecipient",
      "msg": "Insurance fund withdrawal recipient must be the designated treasury address"
    },
    {
      "code": 6350,
      "name": "spotDlobTradingDisabled",
      "msg": "Spot DLOB trading is disabled"
    },
    {
      "code": 6351,
      "name": "invalidAdminTier",
      "msg": "Signer is not authorized for this admin tier"
    },
    {
      "code": 6352,
      "name": "withdrawGuardThresholdNotionalTooLarge",
      "msg": "Withdraw guard threshold notional exceeds max"
    },
    {
      "code": 6353,
      "name": "invalidProtocolFeeRecipient",
      "msg": "Recipient must be the configured protocol fee recipient"
    },
    {
      "code": 6354,
      "name": "insufficientProtocolFees",
      "msg": "Insufficient protocol fees available to withdraw"
    },
    {
      "code": 6355,
      "name": "invalidNativeStateAccount",
      "msg": "Native dispatch: supplied state account is not the canonical Velocity state PDA"
    },
    {
      "code": 6356,
      "name": "invalidNativePerpMarketAccount",
      "msg": "Native dispatch: supplied market account is not a Velocity perp market"
    },
    {
      "code": 6357,
      "name": "isolatedPositionDisabled",
      "msg": "Isolated positions are not enabled in this build"
    },
    {
      "code": 6358,
      "name": "equityBelowFloor",
      "msg": "Account equity is below the user-set equity floor"
    },
    {
      "code": 6359,
      "name": "invalidEquityFloorTransfer",
      "msg": "Invalid equity floor transfer between subaccounts"
    },
    {
      "code": 6360,
      "name": "ifDepositMintsZeroShares",
      "msg": "Insurance fund deposit would mint zero shares"
    },
    {
      "code": 6361,
      "name": "liquidationWorsensAccountHealth",
      "msg": "Liquidation would worsen the account's margin shortage"
    },
    {
      "code": 6362,
      "name": "perpBankruptcyMustPrecedeSpot",
      "msg": "Perp bankruptcies must be resolved before spot bankruptcies"
    },
    {
      "code": 6363,
      "name": "invalidRevenueShareRecipient",
      "msg": "Revenue share recipient user must be sub_account_id 0"
    },
    {
      "code": 6364,
      "name": "dailyDepositLimit",
      "msg": "Spot market daily deposit limit hit"
    },
    {
      "code": 6365,
      "name": "reservedSpotMarketName",
      "msg": "The name 'USDT' is reserved for the quote spot market (index 0)"
    },
    {
      "code": 6366,
      "name": "cannotModifyBuilderOrder",
      "msg": "Cannot modify a builder-coded order; cancel and re-place instead"
    },
    {
      "code": 6367,
      "name": "invalidAccountExtension",
      "msg": "Invalid account extension"
    },
    {
      "code": 6368,
      "name": "invalidEquityBreakerReset",
      "msg": "Invalid equity breaker reset"
    },
    {
      "code": 6369,
      "name": "invalidNativeInstructionData",
      "msg": "Native dispatch: instruction data is malformed for this opcode"
    },
    {
      "code": 6370,
      "name": "mmOracleUpdateDisabled",
      "msg": "MM oracle updates are disabled by the admin feature-bit kill switch"
    },
    {
      "code": 6371,
      "name": "spotMarketInterestStaleForMargin",
      "msg": "Spot market interest is too stale to value a borrow for margin"
    },
    {
      "code": 6372,
      "name": "unsettledRevenueShareOnDelist",
      "msg": "Market still owes builder/referrer revenue share; settle it before delisting"
    },
    {
      "code": 6373,
      "name": "revenueShareOrderNotForfeitable",
      "msg": "Revenue share order can still be paid; settle it instead of forfeiting"
    },
    {
      "code": 6374,
      "name": "invalidQuoterConfig",
      "msg": "Quoter registry entry config is invalid"
    },
    {
      "code": 6375,
      "name": "invalidQuoterAuthority",
      "msg": "Signer does not control this quoter registry entry"
    },
    {
      "code": 6376,
      "name": "insufficientCrankReservoir",
      "msg": "CLOB crank condition account cannot cover the keeper payment"
    },
    {
      "code": 6377,
      "name": "orderPlacedOnClob",
      "msg": "Order is placed on the CLOB; cancel it there (cancel_clob_order)"
    },
    {
      "code": 6378,
      "name": "orderAwaitingTriggerRecross",
      "msg": "Trigger is awaiting a price recross after eviction"
    },
    {
      "code": 6379,
      "name": "crossMatchImbalanced",
      "msg": "Cross match legs are imbalanced"
    },
    {
      "code": 6380,
      "name": "crossMatchUnprofitable",
      "msg": "Cross match is not profitable after fees"
    },
    {
      "code": 6381,
      "name": "unattestedFastActivation",
      "msg": "Faster-than-default activation requires the flow-authority attestation"
    },
    {
      "code": 6382,
      "name": "invalidQuoterResponse",
      "msg": "Quoter returned a malformed quote/execute response"
    },
    {
      "code": 6383,
      "name": "quoterOverfilled",
      "msg": "Quoter filled more base than the router allocated to it"
    },
    {
      "code": 6384,
      "name": "quoterFillOffQuote",
      "msg": "Quoter filled at a price its quote does not support"
    },
    {
      "code": 6385,
      "name": "quoterSubjectNotPermitted",
      "msg": "Quoter returned a balance change for a user it may not act against"
    },
    {
      "code": 6386,
      "name": "tooManyQuoterWireUsers",
      "msg": "More loaded users than the quoter wire can carry"
    },
    {
      "code": 6387,
      "name": "signedRouteMismatch",
      "msg": "Claimed route does not match the one the order was signed with"
    },
    {
      "code": 6388,
      "name": "signedRouteEntryMissing",
      "msg": "A quoter the order's signed route names is absent from the fill"
    },
    {
      "code": 6389,
      "name": "crossedTakerRemainderPending",
      "msg": "A crossed taker remainder must be resolved by crank_taker_origin_cross"
    },
    {
      "code": 6390,
      "name": "noTakerOriginCross",
      "msg": "No resolvable taker-origin cross on this book"
    },
    {
      "code": 6391,
      "name": "takerOriginCrossWorseForTaker",
      "msg": "Crossing would leave the taker worse off than its resting price"
    },
    {
      "code": 6392,
      "name": "insufficientCrankTreasury",
      "msg": "Crank treasury has too few lamports for this payout"
    },
    {
      "code": 6393,
      "name": "crankReservoirNotLow",
      "msg": "Crank reservoir is above its refill watermark"
    }
  ],
  "types": [
    {
      "name": "amm",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "feePool",
            "docs": [
              "partition of fees from perp market trading moved from pnl settlements"
            ],
            "type": {
              "defined": {
                "name": "poolBalance"
              }
            }
          },
          {
            "name": "baseAssetReserve",
            "docs": [
              "`x` reserves for constant product mm formula (x * y = k)",
              "precision: AMM_RESERVE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "quoteAssetReserve",
            "docs": [
              "`y` reserves for constant product mm formula (x * y = k)",
              "precision: AMM_RESERVE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "concentrationCoef",
            "docs": [
              "determines how close the min/max base asset reserve sit vs base reserves",
              "allow for decreasing slippage without increasing liquidity and v.v.",
              "precision: PERCENTAGE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "minBaseAssetReserve",
            "docs": [
              "minimum base_asset_reserve allowed before AMM is unavailable",
              "precision: AMM_RESERVE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "maxBaseAssetReserve",
            "docs": [
              "maximum base_asset_reserve allowed before AMM is unavailable",
              "precision: AMM_RESERVE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "sqrtK",
            "docs": [
              "`sqrt(k)` in constant product mm formula (x * y = k). stored to avoid velocity caused by integer math issues",
              "precision: AMM_RESERVE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "pegMultiplier",
            "docs": [
              "normalizing numerical factor for y, its use offers lowest slippage in cp-curve when market is balanced",
              "precision: PEG_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "terminalQuoteAssetReserve",
            "docs": [
              "y when market is balanced. stored to save computation",
              "precision: AMM_RESERVE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "baseAssetAmountWithAmm",
            "docs": [
              "tracks net position (longs-shorts) in market with AMM as counterparty",
              "precision: BASE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "totalFee",
            "docs": [
              "Lifetime fee-derived income booked to the AMM ITSELF (analytics):",
              "its fee provision (the `amm_fee` cut of trade-fee remainders) plus",
              "spread surplus. NOT the market's gross fees — those live in",
              "`PerpMarket.fee_ledger.total_exchange_fee`. Adjusted in lockstep with",
              "`total_fee_minus_distributions` by admin summary-stats corrections.",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "totalMmFee",
            "docs": [
              "Spread-capture component of `total_fee` (analytics): the gap between",
              "the curve price and the execution price on AMM fills. Trading profit,",
              "not a fee anyone explicitly pays.",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "totalFeeMinusDistributions",
            "docs": [
              "The AMM's equity ledger (retained earnings) — broader than the name",
              "suggests: fee income (`apply_fill_fees`) + funding and other P&L",
              "(`record_amm_pnl`) + external credits (`record_credit`), minus",
              "curve-adjustment costs (`apply_cost`) and bankruptcy clawbacks.",
              "Contains ONLY the AMM's own money (protocol/IF carveouts never enter",
              "it). Drives `is_underwater`, the drawdown breaker, and curve-cost",
              "budgets; reconciled against pool balances by",
              "`calculate_perp_market_amm_summary_stats`.",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "totalFeeWithdrawn",
            "docs": [
              "@deprecated frozen analytics counter from the pre-isolation design",
              "(sum of fees withdrawn from the fee pool to the revenue pool). The",
              "sweep no longer touches the AMM's pools, so nothing writes this.",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "askBaseAssetReserve",
            "docs": [
              "Cached spread-adjusted reserves for the ask (long-take) side, derived",
              "from `long_spread` + `reference_price_offset`. Refreshed by",
              "[`crate::vlp::amm::math::spread::update_amm_quote_state`] on every AMM crank",
              "/ fill `setup`; quote/fill paths read these directly instead of",
              "recomputing per quote. Also surfaced to dashboards/tracking.",
              "precision: AMM_RESERVE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "askQuoteAssetReserve",
            "docs": [
              "precision: AMM_RESERVE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "bidBaseAssetReserve",
            "docs": [
              "Cached spread-adjusted reserves for the bid (short-take) side.",
              "precision: AMM_RESERVE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "bidQuoteAssetReserve",
            "docs": [
              "precision: AMM_RESERVE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "lastUpdateSlot",
            "docs": [
              "the last blockchain slot the amm was updated"
            ],
            "type": "u64"
          },
          {
            "name": "netRevenueSinceLastFunding",
            "docs": [
              "the total_fee_minus_distribution change since the last funding update",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "lastCumulativeFundingRateLong",
            "docs": [
              "AMM's last-seen cumulative funding rates. Mirrors",
              "`PerpPosition::last_cumulative_funding_rate` on user positions —",
              "the AMM settles its own funding payment from",
              "`(market.cumulative_funding_rate_* − own_last) ×",
              "counterparty_position`, same math shape user positions use. The",
              "AMM is the counterparty for the net imbalance, so the relevant",
              "cum rate is the LONG one when the AMM is net long",
              "(base_asset_amount_with_amm < 0, i.e. users net short) and the",
              "SHORT one when the AMM is net short."
            ],
            "type": "i64"
          },
          {
            "name": "lastCumulativeFundingRateShort",
            "type": "i64"
          },
          {
            "name": "lastOracleReservePriceSpreadPct",
            "docs": [
              "Cached oracle-vs-reserve price spread (signed, BID_ASK_SPREAD_PRECISION),",
              "the spread input that seeds `calculate_spread`. Refreshed alongside the",
              "other cached spread fields by `update_amm_quote_state`."
            ],
            "type": "i64"
          },
          {
            "name": "lastSpreadUpdateSlot",
            "docs": [
              "Blockchain slot at which the cached spread state (`long_spread`,",
              "`short_spread`, `reference_price_offset`, the ask/bid reserves, and",
              "`last_oracle_reserve_price_spread_pct`) was last refreshed. Lets",
              "quote paths skip recompute within a slot and lets dashboards reason",
              "about cache staleness independently of `last_update_slot`."
            ],
            "type": "u64"
          },
          {
            "name": "baseSpread",
            "docs": [
              "the minimum spread the AMM can quote. also used as step size for some spread logic increases."
            ],
            "type": "u32"
          },
          {
            "name": "maxSpread",
            "docs": [
              "the maximum spread the AMM can quote"
            ],
            "type": "u32"
          },
          {
            "name": "longSpread",
            "docs": [
              "Cached spread applied to the ask (long-take) side, in",
              "BID_ASK_SPREAD_PRECISION. Refreshed by `update_amm_quote_state`."
            ],
            "type": "u32"
          },
          {
            "name": "shortSpread",
            "docs": [
              "Cached spread applied to the bid (short-take) side, in",
              "BID_ASK_SPREAD_PRECISION. Refreshed by `update_amm_quote_state`."
            ],
            "type": "u32"
          },
          {
            "name": "referencePriceOffset",
            "docs": [
              "Cached reference-price offset (signed, PRICE_PRECISION) applied to both",
              "sides' quotes. Refreshed by `update_amm_quote_state`."
            ],
            "type": "i32"
          },
          {
            "name": "maxFillReserveFraction",
            "docs": [
              "the fraction of total available liquidity a single fill on the AMM can consume"
            ],
            "type": "u16"
          },
          {
            "name": "maxSlippageRatio",
            "docs": [
              "the maximum slippage a single fill on the AMM can push"
            ],
            "type": "u16"
          },
          {
            "name": "curveUpdateIntensity",
            "docs": [
              "the update intensity of AMM formulaic updates (adjusting k). 0-100"
            ],
            "type": "u8"
          },
          {
            "name": "ammJitIntensity",
            "docs": [
              "the jit intensity of AMM. larger intensity means larger participation in jit. 0 means no jit participation.",
              "(0, 100] is intensity for protocol-owned AMM."
            ],
            "type": "u8"
          },
          {
            "name": "ammSpreadAdjustment",
            "docs": [
              "signed scale amm_spread similar to fee_adjustment logic (-100 = 0, 100 = double)"
            ],
            "type": "i8"
          },
          {
            "name": "ammInventorySpreadAdjustment",
            "docs": [
              "signed scale amm_spread similar to fee_adjustment logic (-100 = 0, 100 = double)"
            ],
            "type": "i8"
          },
          {
            "name": "referencePriceOffsetDeadbandPct",
            "type": "u8"
          },
          {
            "name": "fundingBiasSensitivity",
            "docs": [
              "s in the funding bias β(f) = 1 + s * ρ(f): how much the paying-side",
              "spread widens while the vAMM pays funding on its inventory.",
              "",
              "s is stored in hundredths (s = value / 100), so at full ramp (ρ = 1)",
              "the multiplier is 1 + value/100: 50 => 1.5x, 100 => 2x, u8 caps s at",
              "2.55. Same convention as `amm_spread_adjustment` (100 = double).",
              "0 disables the bias."
            ],
            "type": "u8"
          },
          {
            "name": "paddingPostAmm",
            "type": {
              "array": [
                "u8",
                2
              ]
            }
          }
        ]
      }
    },
    {
      "name": "addAmmConstituentMappingDatum",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "constituentIndex",
            "type": "u16"
          },
          {
            "name": "perpMarketIndex",
            "type": "u16"
          },
          {
            "name": "weight",
            "type": "i64"
          }
        ]
      }
    },
    {
      "name": "ammAccountMeta",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "pubkey",
            "type": "pubkey"
          },
          {
            "name": "isWritable",
            "docs": [
              "Whether the account is passed writable to the quoter program.",
              "`is_signer` is intentionally not stored: the only slot a quoter CPI",
              "ever receives signer privilege on is `quoter_signer`, decided by",
              "pubkey match rather than by registration (see [`quoter_account_metas`])."
            ],
            "type": "bool"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                7
              ]
            }
          }
        ]
      }
    },
    {
      "name": "ammCache",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "bump",
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                3
              ]
            }
          },
          {
            "name": "cache",
            "type": {
              "vec": {
                "defined": {
                  "name": "cacheInfo"
                }
              }
            }
          }
        ]
      }
    },
    {
      "name": "ammConstituentDatum",
      "serialization": "bytemuck",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "perpMarketIndex",
            "type": "u16"
          },
          {
            "name": "constituentIndex",
            "type": "u16"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                4
              ]
            }
          },
          {
            "name": "lastSlot",
            "type": "u64"
          },
          {
            "name": "weight",
            "docs": [
              "PERCENTAGE_PRECISION. The weight this constituent has on the perp market"
            ],
            "type": "i64"
          }
        ]
      }
    },
    {
      "name": "ammConstituentMapping",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "lpPool",
            "type": "pubkey"
          },
          {
            "name": "bump",
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                3
              ]
            }
          },
          {
            "name": "weights",
            "type": {
              "vec": {
                "defined": {
                  "name": "ammConstituentDatum"
                }
              }
            }
          }
        ]
      }
    },
    {
      "name": "ammCurveChanged",
      "docs": [
        "AMM-side curve change: peg / reserves / sqrt_k moved, and the AMM",
        "debited the adjustment cost from its books. Emitted by the AMM itself",
        "(from `on_market_event(FundingUpdated)`'s k-update branch, from",
        "`snap_to_oracle`, and from the admin `repeg` ix). PerpMarket-side",
        "state at the time of the change (`base_asset_amount_long/short`,",
        "`number_of_users`) lives on a separate `FundingRateRecord` or can be",
        "queried by consumers correlating on `(ts, market_index)`."
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "pegMultiplierBefore",
            "type": "u128"
          },
          {
            "name": "baseAssetReserveBefore",
            "type": "u128"
          },
          {
            "name": "quoteAssetReserveBefore",
            "type": "u128"
          },
          {
            "name": "sqrtKBefore",
            "type": "u128"
          },
          {
            "name": "pegMultiplierAfter",
            "type": "u128"
          },
          {
            "name": "baseAssetReserveAfter",
            "type": "u128"
          },
          {
            "name": "quoteAssetReserveAfter",
            "type": "u128"
          },
          {
            "name": "sqrtKAfter",
            "type": "u128"
          },
          {
            "name": "adjustmentCost",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "totalFeeMinusDistributionsAfter",
            "docs": [
              "precision: QUOTE_PRECISION — AMM's TFMD after the change."
            ],
            "type": "i128"
          },
          {
            "name": "oraclePrice",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          }
        ]
      }
    },
    {
      "name": "assetTier",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "collateral"
          },
          {
            "name": "protected"
          },
          {
            "name": "cross"
          },
          {
            "name": "isolated"
          },
          {
            "name": "unlisted"
          }
        ]
      }
    },
    {
      "name": "builderInfo",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "authority",
            "type": "pubkey"
          },
          {
            "name": "maxFeeTenthBps",
            "type": "u16"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                6
              ]
            }
          }
        ]
      }
    },
    {
      "name": "cacheInfo",
      "serialization": "bytemuck",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "oracle",
            "type": "pubkey"
          },
          {
            "name": "lastFeePoolTokenAmount",
            "type": "u128"
          },
          {
            "name": "lastNetPnlPoolTokenAmount",
            "type": "i128"
          },
          {
            "name": "lastExchangeFees",
            "type": "u128"
          },
          {
            "name": "lastSettleAmmExFees",
            "type": "u128"
          },
          {
            "name": "lastSettleAmmPnl",
            "type": "i128"
          },
          {
            "name": "position",
            "docs": [
              "BASE PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "slot",
            "type": "u64"
          },
          {
            "name": "lastSettleAmount",
            "type": "u64"
          },
          {
            "name": "lastSettleSlot",
            "type": "u64"
          },
          {
            "name": "lastSettleTs",
            "type": "i64"
          },
          {
            "name": "quoteOwedFromLpPool",
            "type": "i64"
          },
          {
            "name": "ammInventoryLimit",
            "type": "i64"
          },
          {
            "name": "oraclePrice",
            "type": "i64"
          },
          {
            "name": "oracleSlot",
            "type": "u64"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "oracleSource",
            "type": "u8"
          },
          {
            "name": "oracleValidity",
            "type": "u8"
          },
          {
            "name": "lpStatusForPerpMarket",
            "type": "u8"
          },
          {
            "name": "ammPositionScalar",
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                34
              ]
            }
          }
        ]
      }
    },
    {
      "name": "cancelAllClobOrdersParams",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "sides",
            "type": {
              "defined": {
                "name": "cancelSidesV0"
              }
            }
          }
        ]
      }
    },
    {
      "name": "cancelClobOrderParams",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "orderRef",
            "docs": [
              "The hint returned at placement; the CLOB fails closed on a stale one."
            ],
            "type": {
              "defined": {
                "name": "clobOrderRefV0"
              }
            }
          }
        ]
      }
    },
    {
      "name": "cancelSidesV0",
      "docs": [
        "Which sides a `cancel_all_v0` withdraws.",
        "",
        "Named sides rather than a pair of bools, because the wire must not be able",
        "to express \"neither\" — that is a maker believing their quotes are gone",
        "when nothing happened.",
        "",
        "What the sides *mean* differs by who is reading: a book walks them as book",
        "sides, a caller unwinds them as position directions, a spline reads them as",
        "taker directions. Each program adds that reading itself; the tags are the",
        "part that has to agree."
      ],
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "bids"
          },
          {
            "name": "asks"
          },
          {
            "name": "both"
          }
        ]
      }
    },
    {
      "name": "clobCrankConditionsV0",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "relay",
            "docs": [
              "Everything relay needs hosted, in one field: the `relay-spec` header,",
              "the condition slots, and the resolver account list every condition",
              "here points at. First field, so its watch offset is 8."
            ],
            "type": {
              "defined": {
                "name": "relayBlock2x8",
                "generics": [
                  {
                    "kind": "const",
                    "value": "2"
                  },
                  {
                    "kind": "const",
                    "value": "8"
                  }
                ]
              }
            }
          },
          {
            "name": "oracle",
            "docs": [
              "The market's oracle, captured at attach time. Resolvers hold only",
              "four fixed accounts, so the staged executor's map section is derived",
              "from here rather than from the perp market account; an admin oracle",
              "rotation goes live for the cranks on re-attach."
            ],
            "type": "pubkey"
          },
          {
            "name": "crankPayments",
            "docs": [
              "Lamports each executor pays its keeper, mirrored into that crank's",
              "`min_payment`. This account doubles as the reservoir those lamports",
              "come from: relay's `assert_paid_v0` measures the keeper's lamport",
              "balance, so a crank that moves no lamports cannot express a fee, and",
              "turners would have no signal to prioritize (or decline) the work. Held",
              "here rather than in a global PDA because the executor already has to",
              "touch this account to repair the expiry hint — so the reservoir costs",
              "no extra account in a crank transaction.",
              "",
              "Refilled by the maker, not the protocol: the flat removal reward the",
              "maker pays accrues to a protocol-owned `User`, and a hot role withdraws",
              "that quote and converts it to SOL to top these reservoirs off. An empty",
              "reservoir stops cranks rather than silently paying nothing, which is the",
              "failure mode ops can actually see."
            ],
            "type": {
              "defined": {
                "name": "crankPaymentsV0"
              }
            }
          },
          {
            "name": "minCrossSurplus",
            "docs": [
              "Floor on the protocol's quote surplus from a cross-match crank, in",
              "QUOTE_PRECISION. A cross costs the protocol real SOL — the reservoir",
              "pays `crank_payments.cross` to whoever cranked it — so a cross that",
              "clears by a cent is a cross worth declining. Zero keeps the bare",
              "\"strictly profitable\" rule.",
              "",
              "Denominated in quote rather than derived from the lamport cost because",
              "the conversion needs a SOL price, and the cross crank carries no SOL",
              "oracle (it holds the perp's oracle and its map section, nothing more).",
              "Admins set it to cover the cross payout with margin and re-price it",
              "alongside the payments, which is the same cadence."
            ],
            "type": "u64"
          },
          {
            "name": "clobBlockOffset",
            "docs": [
              "Where the book's own condition block sits in the market account, as it",
              "reported at attach.",
              "",
              "A market has two blocks and each needs its own relay watch: this",
              "account's, whose block is its first field at offset 8, and the book's,",
              "which holds the four conditions describing the book itself. A",
              "registrar that watches only this account leaves the book's cranks",
              "unwoken, so the offset is captured here for it to find."
            ],
            "type": "u32"
          },
          {
            "name": "topOfBookOffset",
            "docs": [
              "The region of the book that changes whenever either side's best moves,",
              "as the book reported it at attach.",
              "",
              "A crossing order is by definition a new best, so a relay watch here",
              "catches every cross the moment it appears. Captured rather than",
              "derived: the book answers where its own heads sit, so velocity",
              "registers a watch on it without knowing its layout. Read by the",
              "per-quoter cross conditions, which watch this same book for a cross",
              "against a PropAMM."
            ],
            "type": "u32"
          },
          {
            "name": "topOfBookLen",
            "type": "u32"
          },
          {
            "name": "marketIndex",
            "docs": [
              "The perp market these conditions crank. Also the PDA seed."
            ],
            "type": "u16"
          },
          {
            "name": "quoteSpotMarketIndex",
            "docs": [
              "The market's quote spot market, captured at attach time (the staged",
              "executor's map section needs its PDA)."
            ],
            "type": "u16"
          },
          {
            "name": "refillWatermarkLamports",
            "docs": [
              "The spendable balance this reservoir wakes its refill at, in lamports.",
              "",
              "Resolved at attach from the treasury's watermark setting and this",
              "market's dearest crank, and stored because it is the threshold the",
              "wake condition carries: relay compares the mirror against this number,",
              "so the executor has to read the same one rather than recompute it. A",
              "figure recomputed from a program constant would drift from the",
              "conditions written before an upgrade, and a market would wake at one",
              "level while its executor refused at another."
            ],
            "type": "u64"
          },
          {
            "name": "spendableMirror",
            "docs": [
              "This account's spendable lamports — its balance less its rent",
              "exemption — as of the last payment or refill.",
              "",
              "A relay watch reads account data, and a lamport balance is account",
              "metadata rather than data. Mirroring it here is what lets the refill",
              "condition wake on a draining reservoir. The write costs nothing: every",
              "payment already writes this account.",
              "",
              "Advisory, not authoritative. The refill instruction reads the real",
              "balance, and the resolver refuses to stage one against a reservoir that",
              "is genuinely full.",
              "",
              "Written by the attach and by every payment, which is every way the",
              "balance falls. A plain lamport transfer into the reservoir is the one",
              "way it can rise without a write, and that leaves the mirror low: the",
              "condition then stays due and turners keep resolving it to \"no work\"",
              "until the next payment restates it. That costs simulations rather than",
              "lamports, and the treasury refill exists so that hand-funding a",
              "reservoir is not the normal path."
            ],
            "type": "u64"
          },
          {
            "name": "padding",
            "docs": [
              "Tail reserve: 4 bytes of alignment slack plus room for a captured",
              "pubkey and change, so a resolver that needs another fixed account can",
              "take it from here instead of forcing an `extend_account` migration on",
              "every market's conditions."
            ],
            "type": {
              "array": [
                "u8",
                16
              ]
            }
          }
        ]
      }
    },
    {
      "name": "clobOrderRefV0",
      "docs": [
        "Order handle: an O(1) node hint verified against the order id, so a stale",
        "hint (node freed or reused) fails closed rather than acting on whichever",
        "order took the slot.",
        "",
        "The `Clob` prefix is load-bearing and stutters here on purpose. This is the",
        "one type on this wire that reaches velocity's *instruction* arguments, so",
        "it is the one that lands in velocity's IDL — beside `Order`, `OrderType`",
        "and `OrderParams`, where a bare `OrderRefV0` names no program. Anchor takes",
        "the declared name, not the alias, so renaming it here renames it there."
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "nodeIndex",
            "type": "u32"
          },
          {
            "name": "orderId",
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "constituent",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "pubkey",
            "docs": [
              "address of the constituent"
            ],
            "type": "pubkey"
          },
          {
            "name": "mint",
            "type": "pubkey"
          },
          {
            "name": "lpPool",
            "type": "pubkey"
          },
          {
            "name": "vault",
            "type": "pubkey"
          },
          {
            "name": "totalSwapFees",
            "docs": [
              "total fees received by the constituent. Positive = fees received, Negative = fees paid"
            ],
            "type": "i128"
          },
          {
            "name": "spotBalance",
            "docs": [
              "spot borrow-lend balance for constituent"
            ],
            "type": {
              "defined": {
                "name": "constituentSpotBalance"
              }
            }
          },
          {
            "name": "lastSpotBalanceTokenAmount",
            "type": "i64"
          },
          {
            "name": "cumulativeSpotInterestAccruedTokenAmount",
            "type": "i64"
          },
          {
            "name": "maxWeightDeviation",
            "docs": [
              "max deviation from target_weight allowed for the constituent",
              "precision: PERCENTAGE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "swapFeeMin",
            "docs": [
              "min fee charged on swaps to/from this constituent",
              "precision: PERCENTAGE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "swapFeeMax",
            "docs": [
              "max fee charged on swaps to/from this constituent",
              "precision: PERCENTAGE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "maxBorrowTokenAmount",
            "docs": [
              "Max Borrow amount:",
              "precision: token precision"
            ],
            "type": "u64"
          },
          {
            "name": "vaultTokenBalance",
            "docs": [
              "ata token balance in token precision"
            ],
            "type": "u64"
          },
          {
            "name": "lastOraclePrice",
            "type": "i64"
          },
          {
            "name": "lastOracleSlot",
            "type": "u64"
          },
          {
            "name": "oracleStalenessThreshold",
            "docs": [
              "Delay allowed for valid AUM calculation"
            ],
            "type": "u64"
          },
          {
            "name": "flashLoanInitialTokenAmount",
            "type": "u64"
          },
          {
            "name": "nextSwapId",
            "docs": [
              "Every swap to/from this constituent has a monotonically increasing id. This is the next id to use"
            ],
            "type": "u64"
          },
          {
            "name": "derivativeWeight",
            "docs": [
              "percentable of derivatve weight to go to this specific derivative PERCENTAGE_PRECISION. Zero if no derivative weight"
            ],
            "type": "u64"
          },
          {
            "name": "volatility",
            "type": "u64"
          },
          {
            "name": "constituentDerivativeDepegThreshold",
            "type": "u64"
          },
          {
            "name": "constituentDerivativeIndex",
            "docs": [
              "The `constituent_index` of the parent constituent. -1 if it is a parent index",
              "Example: if in a pool with SOL (parent) and dSOL (derivative),",
              "SOL.constituent_index = 1, SOL.constituent_derivative_index = -1,",
              "dSOL.constituent_index = 2, dSOL.constituent_derivative_index = 1"
            ],
            "type": "i16"
          },
          {
            "name": "spotMarketIndex",
            "type": "u16"
          },
          {
            "name": "constituentIndex",
            "type": "u16"
          },
          {
            "name": "decimals",
            "type": "u8"
          },
          {
            "name": "bump",
            "type": "u8"
          },
          {
            "name": "vaultBump",
            "type": "u8"
          },
          {
            "name": "gammaInventory",
            "type": "u8"
          },
          {
            "name": "gammaExecution",
            "type": "u8"
          },
          {
            "name": "xi",
            "type": "u8"
          },
          {
            "name": "status",
            "type": "u8"
          },
          {
            "name": "pausedOperations",
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                170
              ]
            }
          }
        ]
      }
    },
    {
      "name": "constituentCorrelations",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "lpPool",
            "type": "pubkey"
          },
          {
            "name": "bump",
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                3
              ]
            }
          },
          {
            "name": "correlations",
            "type": {
              "vec": "i64"
            }
          }
        ]
      }
    },
    {
      "name": "constituentParams",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "maxWeightDeviation",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "swapFeeMin",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "swapFeeMax",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "maxBorrowTokenAmount",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "oracleStalenessThreshold",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "costToTradeBps",
            "type": {
              "option": "i32"
            }
          },
          {
            "name": "constituentDerivativeIndex",
            "type": {
              "option": "i16"
            }
          },
          {
            "name": "derivativeWeight",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "volatility",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "gammaExecution",
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "gammaInventory",
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "xi",
            "type": {
              "option": "u8"
            }
          }
        ]
      }
    },
    {
      "name": "constituentSpotBalance",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "scaledBalance",
            "docs": [
              "The scaled balance of the position. To get the token amount, multiply by the cumulative deposit/borrow",
              "interest of corresponding market.",
              "precision: token precision"
            ],
            "type": "u128"
          },
          {
            "name": "cumulativeDeposits",
            "docs": [
              "The cumulative deposits/borrows a user has made into a market",
              "precision: token mint precision"
            ],
            "type": "i64"
          },
          {
            "name": "marketIndex",
            "docs": [
              "The market index of the corresponding spot market"
            ],
            "type": "u16"
          },
          {
            "name": "balanceType",
            "docs": [
              "Whether the position is deposit or borrow"
            ],
            "type": {
              "defined": {
                "name": "spotBalanceType"
              }
            }
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                5
              ]
            }
          }
        ]
      }
    },
    {
      "name": "constituentTargetBase",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "lpPool",
            "type": "pubkey"
          },
          {
            "name": "bump",
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                3
              ]
            }
          },
          {
            "name": "targets",
            "type": {
              "vec": {
                "defined": {
                  "name": "targetsDatum"
                }
              }
            }
          }
        ]
      }
    },
    {
      "name": "contractTier",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "a"
          },
          {
            "name": "b"
          },
          {
            "name": "c"
          },
          {
            "name": "speculative"
          },
          {
            "name": "highlySpeculative"
          },
          {
            "name": "isolated"
          }
        ]
      }
    },
    {
      "name": "contractType",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "perpetual"
          },
          {
            "name": "deprecatedFuture"
          },
          {
            "name": "deprecatedPrediction"
          }
        ]
      }
    },
    {
      "name": "crankCostUnitsV0",
      "docs": [
        "Cost units each of a market's cranks requests, one field per crank.",
        "",
        "Measured, not guessed: a turner simulates the crank and requests a compute",
        "limit from what it burned, and the rest of the sum — signatures, write",
        "locks, instruction-data bytes, the loaded-accounts limit — falls out of the",
        "transaction it assembles. An admin passes those totals here.",
        "",
        "The unit is the block-packing cost unit, which is what the network prices a",
        "transaction by. `State.transaction_fee_rails` turns it into lamports."
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "removal",
            "docs": [
              "`evict_worst` / `remove_expired`: one book write and a hint repair."
            ],
            "type": "u32"
          },
          {
            "name": "cross",
            "docs": [
              "`crank_cross_match`: two settlement legs through the router."
            ],
            "type": "u32"
          },
          {
            "name": "takerOriginCross",
            "docs": [
              "`crank_taker_origin_cross`: the same, against a taker remainder."
            ],
            "type": "u32"
          },
          {
            "name": "trigger",
            "docs": [
              "`trigger_order` / `trigger_clob_order` in program-keeper mode."
            ],
            "type": "u32"
          },
          {
            "name": "liquidation",
            "docs": [
              "`liquidate_perp_with_fill` in program-keeper mode."
            ],
            "type": "u32"
          },
          {
            "name": "forceCancel",
            "docs": [
              "`force_cancel_clob_orders`."
            ],
            "type": "u32"
          },
          {
            "name": "refill",
            "docs": [
              "`refill_crank_reservoir`: one lamport move and a mirror write."
            ],
            "type": "u32"
          }
        ]
      }
    },
    {
      "name": "crankPaymentsV0",
      "docs": [
        "What each of a market's cranks pays its keeper, in lamports.",
        "",
        "One figure per crank rather than one for the market. A book removal and a",
        "two-legged cross differ by an order of magnitude in what they request, and",
        "the network charges a transaction for what it requests — so a single figure",
        "either underpays the cross, and nobody runs it, or overpays every removal.",
        "",
        "Derived once at attach time from [`CrankCostUnitsV0`] and",
        "`State.transaction_fee_rails`. Stored rather than recomputed at crank time",
        "for two reasons: pricing itself would cost a crank compute and an extra",
        "account, and a staged executor that could re-derive its own terms could",
        "re-price its own work."
      ],
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "removal",
            "type": "u32"
          },
          {
            "name": "cross",
            "type": "u32"
          },
          {
            "name": "takerOriginCross",
            "type": "u32"
          },
          {
            "name": "trigger",
            "type": "u32"
          },
          {
            "name": "liquidation",
            "type": "u32"
          },
          {
            "name": "forceCancel",
            "type": "u32"
          },
          {
            "name": "refill",
            "docs": [
              "What the *treasury* pays to have this market's reservoir refilled.",
              "",
              "Stored with the market's other crank prices even though the treasury is",
              "the purse, because this is where the refill condition lives and a",
              "condition has to advertise a floor a turner can filter on. Derived from",
              "the same rails as every other crank, so re-pricing the network",
              "re-prices this too on the market's next attach."
            ],
            "type": "u32"
          },
          {
            "name": "padding",
            "type": "u32"
          }
        ]
      }
    },
    {
      "name": "crankTreasuryV0",
      "docs": [
        "The protocol's lamport pool for relay cranks."
      ],
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "totalPaid",
            "docs": [
              "Lifetime lamports paid to keepers that refilled a reservoir."
            ],
            "type": "u64"
          },
          {
            "name": "totalRefilled",
            "docs": [
              "Lifetime lamports moved out to market reservoirs."
            ],
            "type": "u64"
          },
          {
            "name": "paddingU64",
            "docs": [
              "Reserved."
            ],
            "type": "u64"
          },
          {
            "name": "refillTargetCranks",
            "docs": [
              "Refill a reservoir up to this many of its most expensive crank.",
              "",
              "Read at refill time, so re-tuning it takes effect on every market at",
              "once."
            ],
            "type": "u16"
          },
          {
            "name": "refillWatermarkCranks",
            "docs": [
              "Wake the refill when a reservoir can pay fewer than this many.",
              "",
              "A refill needs two levels or it fills by nothing. This is the low one,",
              "and unlike the target it is *resolved to lamports at attach* and stored",
              "on the market, because it is the threshold relay compares the mirrored",
              "balance against and a condition carries its own threshold. Changing it",
              "therefore reaches a market on its next attach.",
              "",
              "Size it for the refill's own round trip. The refill is itself a relay",
              "crank — polled for, simulated, then landed — and the reservoir goes on",
              "paying for ordinary work throughout. Both terms are worst together: a",
              "market-wide move is when cranks fire fastest and when the network is",
              "slowest to land one, and a reservoir that runs dry stops cranking at",
              "exactly that point with nothing else to report it."
            ],
            "type": "u16"
          },
          {
            "name": "padding",
            "docs": [
              "Tail reserve, so a later field costs no migration."
            ],
            "type": {
              "array": [
                "u8",
                36
              ]
            }
          }
        ]
      }
    },
    {
      "name": "deleteUserRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "docs": [
              "unix_timestamp of action"
            ],
            "type": "i64"
          },
          {
            "name": "userAuthority",
            "type": "pubkey"
          },
          {
            "name": "user",
            "type": "pubkey"
          },
          {
            "name": "subAccountId",
            "type": "u16"
          },
          {
            "name": "keeper",
            "type": {
              "option": "pubkey"
            }
          }
        ]
      }
    },
    {
      "name": "depositDirection",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "deposit"
          },
          {
            "name": "withdraw"
          }
        ]
      }
    },
    {
      "name": "depositExplanation",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "none"
          },
          {
            "name": "transfer"
          },
          {
            "name": "borrow"
          },
          {
            "name": "repayBorrow"
          },
          {
            "name": "reward"
          }
        ]
      }
    },
    {
      "name": "depositRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "docs": [
              "unix_timestamp of action"
            ],
            "type": "i64"
          },
          {
            "name": "userAuthority",
            "type": "pubkey"
          },
          {
            "name": "user",
            "docs": [
              "user account public key"
            ],
            "type": "pubkey"
          },
          {
            "name": "direction",
            "type": {
              "defined": {
                "name": "depositDirection"
              }
            }
          },
          {
            "name": "depositRecordId",
            "type": "u64"
          },
          {
            "name": "amount",
            "docs": [
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "marketIndex",
            "docs": [
              "spot market index"
            ],
            "type": "u16"
          },
          {
            "name": "oraclePrice",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "marketDepositBalance",
            "docs": [
              "precision: SPOT_BALANCE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "marketWithdrawBalance",
            "docs": [
              "precision: SPOT_BALANCE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "marketCumulativeDepositInterest",
            "docs": [
              "precision: SPOT_CUMULATIVE_INTEREST_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "marketCumulativeBorrowInterest",
            "docs": [
              "precision: SPOT_CUMULATIVE_INTEREST_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "totalDepositsAfter",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "totalWithdrawsAfter",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "explanation",
            "type": {
              "defined": {
                "name": "depositExplanation"
              }
            }
          },
          {
            "name": "transferUser",
            "type": {
              "option": "pubkey"
            }
          },
          {
            "name": "signer",
            "type": {
              "option": "pubkey"
            }
          },
          {
            "name": "userTokenAmountAfter",
            "docs": [
              "precision: token mint precision"
            ],
            "type": "i128"
          }
        ]
      }
    },
    {
      "name": "directionV0",
      "docs": [
        "Taker direction, from the taker's perspective.",
        "",
        "Encoded as its discriminant, `Long = 0`, and every program on this wire",
        "reads the same declaration — a taker direction inverted across the",
        "boundary would fill the wrong side of a book."
      ],
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "long"
          },
          {
            "name": "short"
          }
        ]
      }
    },
    {
      "name": "feeLedger",
      "docs": [
        "All of a perp market's fee-split accounting in one ledger.",
        "Pure counters — token claims live in the pools",
        "(`protocol_fee_pool`, the quote `revenue_pool`, `AMM.fee_pool`).",
        "Convention: gross-fee counters record what the taker actually paid",
        "(post referee discount, pre carve-outs) on BOTH the AMM and DLOB-match",
        "paths."
      ],
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "totalExchangeFee",
            "docs": [
              "lifetime gross taker fees collected (analytics; not a routing driver)",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "totalLiquidationFee",
            "docs": [
              "lifetime liquidation fees charged to liquidatees (IF + protocol cuts;",
              "pure analytics — routing happens via the pending counters).",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "pendingProtocolFee",
            "docs": [
              "protocol (residual) carveouts accrued but not yet materialized into",
              "`protocol_fee_pool`. precision: QUOTE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "pendingIfFee",
            "docs": [
              "insurance-fund carveouts accrued but not yet materialized into the",
              "quote `revenue_pool`; also the first bankruptcy tranche.",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "ammProtocolFeesReceived",
            "docs": [
              "cumulative fee provision granted to the AMM via `amm_fee_numerator`,",
              "plus the vAMM maker rebate when `FeatureBitFlags::VammMakerRebate` is",
              "enabled — its backstop-of-last-resort tranche, drawable (and",
              "decremented) only in bankruptcy. Enabling the rebate bit therefore",
              "grows the bankruptcy clawback cap by the rebates earned. The AMM's own",
              "spread/trading capital beyond this provision is never tapped.",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "pendingAmmProvision",
            "docs": [
              "AMM fee provision (including the vAMM maker rebate when enabled)",
              "accrued at fill (already booked into the AMM's",
              "`total_fee_minus_distributions`) but not yet tokenized into",
              "`amm.fee_pool` by the sweep. Invariant: `<= amm_protocol_fees_received`.",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u128"
          }
        ]
      }
    },
    {
      "name": "feeStructure",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "feeTiers",
            "type": {
              "array": [
                {
                  "defined": {
                    "name": "feeTier"
                  }
                },
                10
              ]
            }
          },
          {
            "name": "fillerRewardStructure",
            "type": {
              "defined": {
                "name": "orderFillerRewardStructure"
              }
            }
          },
          {
            "name": "flatFillerFee",
            "type": "u64"
          },
          {
            "name": "ammFeeNumerator",
            "docs": [
              "Share of the trade-fee *remainder* (taker fee after maker rebate, referral,",
              "referee discount, and filler reward are taken off the top) provisioned to",
              "the AMM as liquidity (its backstop-of-last-resort tranche, tracked in",
              "`PerpMarket.fee_ledger.amm_protocol_fees_received` alongside the vAMM",
              "maker rebate when that feature is enabled). precision:",
              "FEE_PERCENTAGE_DENOMINATOR. `amm_fee_numerator + if_fee_numerator` must",
              "be <= FEE_PERCENTAGE_DENOMINATOR; the protocol receives the residual",
              "(`remainder − amm − if`) into its withdrawable `protocol_fee_pool`.",
              "(Was the reserved `padding: u64`, repartitioned into two u32s —",
              "size/alignment unchanged.)"
            ],
            "type": "u32"
          },
          {
            "name": "ifFeeNumerator",
            "docs": [
              "Share of the trade-fee remainder routed to the insurance fund (`revenue_pool`)."
            ],
            "type": "u32"
          }
        ]
      }
    },
    {
      "name": "feeTier",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "feeNumerator",
            "type": "u32"
          },
          {
            "name": "feeDenominator",
            "type": "u32"
          },
          {
            "name": "makerRebateNumerator",
            "type": "u32"
          },
          {
            "name": "makerRebateDenominator",
            "type": "u32"
          },
          {
            "name": "referrerRewardNumerator",
            "type": "u32"
          },
          {
            "name": "referrerRewardDenominator",
            "type": "u32"
          },
          {
            "name": "refereeFeeNumerator",
            "type": "u32"
          },
          {
            "name": "refereeFeeDenominator",
            "type": "u32"
          }
        ]
      }
    },
    {
      "name": "firedConditionArgV0",
      "docs": [
        "Which condition relay is asking about.",
        "",
        "Byte-identical to `relay_spec::FiredConditionV0`, which is what the turner",
        "appends to a resolver's instruction data. Declared here because that crate",
        "carries no borsh derives, and stated in velocity's own types — a `Pubkey`",
        "and a `u32` rather than the byte arrays a `Pod` layout needs — so the IDL",
        "reads as an argument list instead of a blob. `tests::the_fired_condition_is",
        "_what_relay_appends` pins the two encodings together."
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "target",
            "docs": [
              "The account holding the condition block."
            ],
            "type": "pubkey"
          },
          {
            "name": "blockOffset",
            "docs": [
              "Byte offset of that block within the account."
            ],
            "type": "u32"
          },
          {
            "name": "index",
            "docs": [
              "Slot of the condition within the block."
            ],
            "type": "u8"
          }
        ]
      }
    },
    {
      "name": "forceCancelClobRefV0",
      "docs": [
        "One order the caller wants reclaimed.",
        "",
        "The side is declared rather than read, because a node carries no side of",
        "its own — the book stores it by which list the node is linked into, and",
        "finding that out costs a walk from the head. Declaring it lets the",
        "risk-reducing test run *before* the CPI, so a reducing order is passed",
        "over instead of being cancelled and then reverting the call. The",
        "declaration is not trusted: the removal the CLOB returns carries the real",
        "side and is checked against it."
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "orderRef",
            "type": {
              "defined": {
                "name": "clobOrderRefV0"
              }
            }
          },
          {
            "name": "side",
            "type": {
              "defined": {
                "name": "sideV0"
              }
            }
          }
        ]
      }
    },
    {
      "name": "fundingPaymentRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "userAuthority",
            "type": "pubkey"
          },
          {
            "name": "user",
            "type": "pubkey"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "fundingPayment",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "baseAssetAmount",
            "docs": [
              "precision: BASE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "userLastCumulativeFunding",
            "docs": [
              "precision: FUNDING_RATE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "ammCumulativeFundingLong",
            "docs": [
              "precision: FUNDING_RATE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "ammCumulativeFundingShort",
            "docs": [
              "precision: FUNDING_RATE_PRECISION"
            ],
            "type": "i128"
          }
        ]
      }
    },
    {
      "name": "fundingRateRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "recordId",
            "type": "u64"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "fundingRate",
            "docs": [
              "precision: FUNDING_RATE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "fundingRateLong",
            "docs": [
              "precision: FUNDING_RATE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "fundingRateShort",
            "docs": [
              "precision: FUNDING_RATE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "cumulativeFundingRateLong",
            "docs": [
              "precision: FUNDING_RATE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "cumulativeFundingRateShort",
            "docs": [
              "precision: FUNDING_RATE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "oraclePriceTwap",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "markPriceTwap",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "baseAssetAmountWithAmm",
            "docs": [
              "precision: BASE_PRECISION"
            ],
            "type": "i128"
          }
        ]
      }
    },
    {
      "name": "hedgeConfig",
      "docs": [
        "Per-market configuration of a perp market's relationship to its hedge (LP)",
        "pool: which pool it routes to, whether hedging is enabled, which hedge",
        "operations are paused, and the fee-routing scalars. Admin-set; never mutated",
        "per fill. Embedded at the tail of `PerpMarket` next to `amm` so the whole VLP",
        "region is contiguous."
      ],
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "poolId",
            "docs": [
              "The LP pool this market hedges into (`LPPool.pool_id`)."
            ],
            "type": "u8"
          },
          {
            "name": "status",
            "docs": [
              "Hedging enabled for this market; 0 disables it."
            ],
            "type": "u8"
          },
          {
            "name": "pausedOperations",
            "docs": [
              "Bitflags of paused `ConstituentLpOperation`s."
            ],
            "type": "u8"
          },
          {
            "name": "exchangeFeeExclusionScalar",
            "docs": [
              "Scalar excluding a share of exchange fees from hedge routing."
            ],
            "type": "u8"
          },
          {
            "name": "feeTransferScalar",
            "docs": [
              "Scalar for the share of fees transferred to the hedge pool."
            ],
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                11
              ]
            }
          }
        ]
      }
    },
    {
      "name": "historicalIndexData",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "lastIndexBidPrice",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "lastIndexAskPrice",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "lastIndexPriceTwap",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "lastIndexPriceTwap5min",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "lastIndexPriceTwapTs",
            "docs": [
              "unix_timestamp of last snapshot"
            ],
            "type": "i64"
          }
        ]
      }
    },
    {
      "name": "historicalOracleData",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "lastOraclePrice",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "lastOracleConf",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "lastOracleDelay",
            "docs": [
              "number of slots since last update"
            ],
            "type": "i64"
          },
          {
            "name": "lastOraclePriceTwap",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "lastOraclePriceTwap5min",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "lastOraclePriceTwapTs",
            "docs": [
              "unix_timestamp of last snapshot"
            ],
            "type": "i64"
          }
        ]
      }
    },
    {
      "name": "hotRole",
      "docs": [
        "Purpose-specific hot role keys held on `State`. Each variant maps to one of the",
        "`hot_*` pubkey fields and is used by `State::require_hot` / `hot_key`."
      ],
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "ammCrank"
          },
          {
            "name": "lpCache"
          },
          {
            "name": "lpSwap"
          },
          {
            "name": "lpSettle"
          },
          {
            "name": "featureFlag"
          },
          {
            "name": "fuel"
          },
          {
            "name": "userFlag"
          },
          {
            "name": "vaultDeposit"
          },
          {
            "name": "mmOracleCrank"
          },
          {
            "name": "ammSpreadAdjust"
          },
          {
            "name": "feeWithdraw"
          },
          {
            "name": "accountExtension"
          },
          {
            "name": "flowAuthority"
          }
        ]
      }
    },
    {
      "name": "initializeQuoterArgs",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "quoterType",
            "type": {
              "defined": {
                "name": "quoterType"
              }
            }
          },
          {
            "name": "responseAccount",
            "type": "pubkey"
          },
          {
            "name": "quoteV0Discriminator",
            "type": {
              "array": [
                "u8",
                8
              ]
            }
          },
          {
            "name": "quoteL3V0Discriminator",
            "docs": [
              "Zero when the quoter has no `quote_l3_v0` leg, which is every quoter",
              "that fills from one account."
            ],
            "type": {
              "array": [
                "u8",
                8
              ]
            }
          },
          {
            "name": "executeV0Discriminator",
            "type": {
              "array": [
                "u8",
                8
              ]
            }
          }
        ]
      }
    },
    {
      "name": "insuranceClaim",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "revenueWithdrawSinceLastSettle",
            "docs": [
              "The amount of revenue last settled",
              "Positive if funds left the perp market,",
              "negative if funds were pulled into the perp market",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "maxRevenueWithdrawPerPeriod",
            "docs": [
              "The max amount of revenue that can be withdrawn per period",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "quoteMaxInsurance",
            "docs": [
              "The max amount of insurance that perp market can use to resolve bankruptcy and pnl deficits",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "quoteSettledInsurance",
            "docs": [
              "The amount of insurance that has been used to resolve bankruptcy and pnl deficits",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "lastRevenueWithdrawTs",
            "docs": [
              "The last time revenue was settled in/out of market"
            ],
            "type": "i64"
          }
        ]
      }
    },
    {
      "name": "insuranceFund",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "vault",
            "type": "pubkey"
          },
          {
            "name": "totalShares",
            "type": "u128"
          },
          {
            "name": "userShares",
            "type": "u128"
          },
          {
            "name": "sharesBase",
            "type": "u128"
          },
          {
            "name": "unstakingPeriod",
            "type": "i64"
          },
          {
            "name": "lastRevenueSettleTs",
            "type": "i64"
          },
          {
            "name": "revenueSettlePeriod",
            "docs": [
              "How often `revenue_pool` may settle into the IF vault (seconds)."
            ],
            "type": "i64"
          },
          {
            "name": "ifFeeFactor",
            "docs": [
              "Fraction of spot deposit-interest gains carved out to the insurance fund",
              "(staker-owned). precision: IF_FACTOR_PRECISION. (Was `total_factor`; the",
              "protocol-vs-staker split was removed — the IF is now 100% staker-owned,",
              "so this is purely the staker IF carveout.) A cut too small to reach a",
              "whole unit is carried on `revenue_pool`, not floored away. See",
              "`split_deposit_interest`."
            ],
            "type": "u32"
          },
          {
            "name": "paddingIf",
            "docs": [
              "Was `user_factor` (the old protocol/staker split knob). The IF is now",
              "100% staker-owned, so the split is gone; slot kept as padding."
            ],
            "type": {
              "array": [
                "u8",
                4
              ]
            }
          }
        ]
      }
    },
    {
      "name": "insuranceFundRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "spotMarketIndex",
            "type": "u16"
          },
          {
            "name": "perpMarketIndex",
            "type": "u16"
          },
          {
            "name": "userIfFactor",
            "docs": [
              "precision: PERCENTAGE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "totalIfFactor",
            "docs": [
              "precision: PERCENTAGE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "vaultAmountBefore",
            "docs": [
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "insuranceVaultAmountBefore",
            "docs": [
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "totalIfSharesBefore",
            "type": "u128"
          },
          {
            "name": "totalIfSharesAfter",
            "type": "u128"
          },
          {
            "name": "amount",
            "docs": [
              "precision: token mint precision"
            ],
            "type": "i64"
          }
        ]
      }
    },
    {
      "name": "insuranceFundStake",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "authority",
            "type": "pubkey"
          },
          {
            "name": "ifShares",
            "type": "u128"
          },
          {
            "name": "lastWithdrawRequestShares",
            "type": "u128"
          },
          {
            "name": "ifBase",
            "type": "u128"
          },
          {
            "name": "lastValidTs",
            "type": "i64"
          },
          {
            "name": "lastWithdrawRequestValue",
            "type": "u64"
          },
          {
            "name": "lastWithdrawRequestTs",
            "type": "i64"
          },
          {
            "name": "costBasis",
            "type": "i64"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                14
              ]
            }
          }
        ]
      }
    },
    {
      "name": "insuranceFundStakeRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "userAuthority",
            "type": "pubkey"
          },
          {
            "name": "action",
            "type": {
              "defined": {
                "name": "stakeAction"
              }
            }
          },
          {
            "name": "amount",
            "docs": [
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "insuranceVaultAmountBefore",
            "docs": [
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "ifSharesBefore",
            "type": "u128"
          },
          {
            "name": "userIfSharesBefore",
            "type": "u128"
          },
          {
            "name": "totalIfSharesBefore",
            "type": "u128"
          },
          {
            "name": "ifSharesAfter",
            "type": "u128"
          },
          {
            "name": "userIfSharesAfter",
            "type": "u128"
          },
          {
            "name": "totalIfSharesAfter",
            "type": "u128"
          }
        ]
      }
    },
    {
      "name": "lpBorrowLendDepositRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "slot",
            "type": "u64"
          },
          {
            "name": "spotMarketIndex",
            "type": "u16"
          },
          {
            "name": "constituentIndex",
            "type": "u16"
          },
          {
            "name": "direction",
            "type": {
              "defined": {
                "name": "depositDirection"
              }
            }
          },
          {
            "name": "tokenBalance",
            "type": "i64"
          },
          {
            "name": "lastTokenBalance",
            "type": "i64"
          },
          {
            "name": "interestAccruedTokenAmount",
            "type": "i64"
          },
          {
            "name": "amountDepositWithdraw",
            "type": "u64"
          },
          {
            "name": "lpPool",
            "type": "pubkey"
          }
        ]
      }
    },
    {
      "name": "lpMintRedeemRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "slot",
            "type": "u64"
          },
          {
            "name": "authority",
            "type": "pubkey"
          },
          {
            "name": "description",
            "type": "u8"
          },
          {
            "name": "amount",
            "docs": [
              "precision: continutent mint precision, gross fees"
            ],
            "type": "u128"
          },
          {
            "name": "fee",
            "docs": [
              "precision: fee on amount, constituent market mint precision"
            ],
            "type": "i128"
          },
          {
            "name": "spotMarketIndex",
            "type": "u16"
          },
          {
            "name": "constituentIndex",
            "type": "u16"
          },
          {
            "name": "oraclePrice",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "mint",
            "docs": [
              "token mint"
            ],
            "type": "pubkey"
          },
          {
            "name": "lpAmount",
            "docs": [
              "lp amount, lp mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "lpFee",
            "docs": [
              "lp fee, lp mint precision"
            ],
            "type": "i64"
          },
          {
            "name": "lpPrice",
            "docs": [
              "the fair price of the lp token, PRICE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "mintRedeemId",
            "type": "u64"
          },
          {
            "name": "lastAum",
            "docs": [
              "LPPool last_aum"
            ],
            "type": "u128"
          },
          {
            "name": "lastAumSlot",
            "type": "u64"
          },
          {
            "name": "inMarketCurrentWeight",
            "docs": [
              "PERCENTAGE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "inMarketTargetWeight",
            "type": "i64"
          },
          {
            "name": "lpPool",
            "type": "pubkey"
          }
        ]
      }
    },
    {
      "name": "lpPool",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "pubkey",
            "docs": [
              "address of the vault."
            ],
            "type": "pubkey"
          },
          {
            "name": "mint",
            "type": "pubkey"
          },
          {
            "name": "whitelistMint",
            "type": "pubkey"
          },
          {
            "name": "constituentTargetBase",
            "type": "pubkey"
          },
          {
            "name": "constituentCorrelations",
            "type": "pubkey"
          },
          {
            "name": "maxAum",
            "docs": [
              "The current number of VaultConstituents in the vault, each constituent is pda(LPPool.address, constituent_index)",
              "which constituent is the quote, receives revenue pool distributions. (maybe this should just be implied idx 0)",
              "pub quote_constituent_index: u16,",
              "QUOTE_PRECISION: Max AUM, Prohibit minting new DLP beyond this"
            ],
            "type": "u128"
          },
          {
            "name": "lastAum",
            "docs": [
              "QUOTE_PRECISION: AUM of the vault in USD, updated lazily"
            ],
            "type": "u128"
          },
          {
            "name": "cumulativeQuoteSentToPerpMarkets",
            "docs": [
              "QUOTE PRECISION: Cumulative quotes from settles"
            ],
            "type": "u128"
          },
          {
            "name": "cumulativeQuoteReceivedFromPerpMarkets",
            "type": "u128"
          },
          {
            "name": "totalMintRedeemFeesPaid",
            "docs": [
              "QUOTE_PRECISION: Total fees paid for minting and redeeming LP tokens"
            ],
            "type": "i128"
          },
          {
            "name": "lastAumSlot",
            "docs": [
              "timestamp of last AUM slot"
            ],
            "type": "u64"
          },
          {
            "name": "maxSettleQuoteAmount",
            "type": "u64"
          },
          {
            "name": "padding",
            "docs": [
              "timestamp of last vAMM revenue rebalance"
            ],
            "type": "u64"
          },
          {
            "name": "mintRedeemId",
            "docs": [
              "Every mint/redeem has a monotonically increasing id. This is the next id to use"
            ],
            "type": "u64"
          },
          {
            "name": "settleId",
            "type": "u64"
          },
          {
            "name": "minMintFee",
            "docs": [
              "PERCENTAGE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "tokenSupply",
            "type": "u64"
          },
          {
            "name": "volatility",
            "type": "u64"
          },
          {
            "name": "constituents",
            "type": "u16"
          },
          {
            "name": "quoteConsituentIndex",
            "type": "u16"
          },
          {
            "name": "bump",
            "type": "u8"
          },
          {
            "name": "gammaExecution",
            "type": "u8"
          },
          {
            "name": "xi",
            "type": "u8"
          },
          {
            "name": "targetOracleDelayFeeBpsPer10Slots",
            "type": "u8"
          },
          {
            "name": "targetPositionDelayFeeBpsPer10Slots",
            "type": "u8"
          },
          {
            "name": "lpPoolId",
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                182
              ]
            }
          }
        ]
      }
    },
    {
      "name": "lpSettleRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "recordId",
            "type": "u64"
          },
          {
            "name": "lastTs",
            "type": "i64"
          },
          {
            "name": "lastSlot",
            "type": "u64"
          },
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "slot",
            "type": "u64"
          },
          {
            "name": "perpMarketIndex",
            "type": "u16"
          },
          {
            "name": "settleToLpAmount",
            "type": "i64"
          },
          {
            "name": "perpAmmPnlDelta",
            "type": "i64"
          },
          {
            "name": "perpAmmExFeeDelta",
            "type": "i64"
          },
          {
            "name": "lpAum",
            "type": "u128"
          },
          {
            "name": "lpPrice",
            "type": "u128"
          },
          {
            "name": "lpPool",
            "type": "pubkey"
          }
        ]
      }
    },
    {
      "name": "lpSwapRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "slot",
            "type": "u64"
          },
          {
            "name": "authority",
            "type": "pubkey"
          },
          {
            "name": "outAmount",
            "docs": [
              "precision: out market mint precision, gross fees"
            ],
            "type": "u128"
          },
          {
            "name": "inAmount",
            "docs": [
              "precision: in market mint precision, gross fees"
            ],
            "type": "u128"
          },
          {
            "name": "outFee",
            "docs": [
              "precision: fee on amount_out, in market mint precision"
            ],
            "type": "i128"
          },
          {
            "name": "inFee",
            "docs": [
              "precision: fee on amount_in, out market mint precision"
            ],
            "type": "i128"
          },
          {
            "name": "outSpotMarketIndex",
            "type": "u16"
          },
          {
            "name": "inSpotMarketIndex",
            "type": "u16"
          },
          {
            "name": "outConstituentIndex",
            "type": "u16"
          },
          {
            "name": "inConstituentIndex",
            "type": "u16"
          },
          {
            "name": "outOraclePrice",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "inOraclePrice",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "lastAum",
            "docs": [
              "LPPool last_aum, QUOTE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "lastAumSlot",
            "type": "u64"
          },
          {
            "name": "inMarketCurrentWeight",
            "docs": [
              "PERCENTAGE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "outMarketCurrentWeight",
            "docs": [
              "PERCENTAGE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "inMarketTargetWeight",
            "docs": [
              "PERCENTAGE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "outMarketTargetWeight",
            "docs": [
              "PERCENTAGE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "inSwapId",
            "type": "u64"
          },
          {
            "name": "outSwapId",
            "type": "u64"
          },
          {
            "name": "lpPool",
            "type": "pubkey"
          }
        ]
      }
    },
    {
      "name": "liquidateBorrowForPerpPnlRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "perpMarketIndex",
            "type": "u16"
          },
          {
            "name": "marketOraclePrice",
            "type": "i64"
          },
          {
            "name": "pnlTransfer",
            "type": "u128"
          },
          {
            "name": "liabilityMarketIndex",
            "type": "u16"
          },
          {
            "name": "liabilityPrice",
            "type": "i64"
          },
          {
            "name": "liabilityTransfer",
            "type": "u128"
          }
        ]
      }
    },
    {
      "name": "liquidatePerpPnlForDepositRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "perpMarketIndex",
            "type": "u16"
          },
          {
            "name": "marketOraclePrice",
            "type": "i64"
          },
          {
            "name": "pnlTransfer",
            "type": "u128"
          },
          {
            "name": "assetMarketIndex",
            "type": "u16"
          },
          {
            "name": "assetPrice",
            "type": "i64"
          },
          {
            "name": "assetTransfer",
            "type": "u128"
          }
        ]
      }
    },
    {
      "name": "liquidatePerpRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "oraclePrice",
            "type": "i64"
          },
          {
            "name": "baseAssetAmount",
            "type": "i64"
          },
          {
            "name": "quoteAssetAmount",
            "type": "i64"
          },
          {
            "name": "fillRecordId",
            "type": "u64"
          },
          {
            "name": "userOrderId",
            "type": "u32"
          },
          {
            "name": "liquidatorOrderId",
            "type": "u32"
          },
          {
            "name": "liquidatorFee",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "ifFee",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "protocolFee",
            "docs": [
              "protocol's cut, routed to the perp market's `protocol_fee_pool`",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "liquidateSpotRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "assetMarketIndex",
            "type": "u16"
          },
          {
            "name": "assetPrice",
            "type": "i64"
          },
          {
            "name": "assetTransfer",
            "type": "u128"
          },
          {
            "name": "liabilityMarketIndex",
            "type": "u16"
          },
          {
            "name": "liabilityPrice",
            "type": "i64"
          },
          {
            "name": "liabilityTransfer",
            "docs": [
              "precision: token mint precision"
            ],
            "type": "u128"
          },
          {
            "name": "ifFee",
            "docs": [
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "protocolFee",
            "docs": [
              "protocol's cut, routed to the liability market's `protocol_fee_pool`",
              "precision: token mint precision"
            ],
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "liquidationRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "liquidationType",
            "type": {
              "defined": {
                "name": "liquidationType"
              }
            }
          },
          {
            "name": "user",
            "type": "pubkey"
          },
          {
            "name": "liquidator",
            "type": "pubkey"
          },
          {
            "name": "marginRequirement",
            "type": "u128"
          },
          {
            "name": "totalCollateral",
            "type": "i128"
          },
          {
            "name": "marginFreed",
            "type": "u64"
          },
          {
            "name": "liquidationId",
            "type": "u16"
          },
          {
            "name": "bankrupt",
            "type": "bool"
          },
          {
            "name": "canceledOrderIds",
            "type": {
              "vec": "u32"
            }
          },
          {
            "name": "liquidatePerp",
            "type": {
              "defined": {
                "name": "liquidatePerpRecord"
              }
            }
          },
          {
            "name": "liquidateSpot",
            "type": {
              "defined": {
                "name": "liquidateSpotRecord"
              }
            }
          },
          {
            "name": "liquidateBorrowForPerpPnl",
            "type": {
              "defined": {
                "name": "liquidateBorrowForPerpPnlRecord"
              }
            }
          },
          {
            "name": "liquidatePerpPnlForDeposit",
            "type": {
              "defined": {
                "name": "liquidatePerpPnlForDepositRecord"
              }
            }
          },
          {
            "name": "perpBankruptcy",
            "type": {
              "defined": {
                "name": "perpBankruptcyRecord"
              }
            }
          },
          {
            "name": "spotBankruptcy",
            "type": {
              "defined": {
                "name": "spotBankruptcyRecord"
              }
            }
          },
          {
            "name": "bitFlags",
            "type": "u8"
          }
        ]
      }
    },
    {
      "name": "liquidationType",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "liquidatePerp"
          },
          {
            "name": "liquidateSpot"
          },
          {
            "name": "liquidateBorrowForPerpPnl"
          },
          {
            "name": "liquidatePerpPnlForDeposit"
          },
          {
            "name": "perpBankruptcy"
          },
          {
            "name": "spotBankruptcy"
          }
        ]
      }
    },
    {
      "name": "lpPoolParams",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "maxSettleQuoteAmount",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "volatility",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "gammaExecution",
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "xi",
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "maxAum",
            "type": {
              "option": "u128"
            }
          },
          {
            "name": "whitelistMint",
            "type": {
              "option": "pubkey"
            }
          }
        ]
      }
    },
    {
      "name": "marketStats",
      "docs": [
        "Historic market data shared across all makers, updated on every fill",
        "regardless of which maker filled (vAMM, DLOB resting order, JIT participant,",
        "future quoter types). Holds mark/oracle TWAPs, rolling std, volume,",
        "intensity, mm-oracle snapshot, `historical_oracle_data`,",
        "`last_oracle_normalised_price`, `last_oracle_valid`.",
        "",
        "Update-cadence rule: anything that needs to refresh on every market event",
        "lives here. Anything AMM-private (reserves, peg, spreads — only matters",
        "when the AMM specifically is the counterparty) lives on `AMM`. See",
        "`docs/amm-decoupling-and-maker-interface.md`."
      ],
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "lastMarkPriceTwap",
            "docs": [
              "Average estimate of (bid+ask)/2 price over funding_period.",
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "lastMarkPriceTwap5min",
            "docs": [
              "Average estimate of (bid+ask)/2 price over FIVE_MINUTES."
            ],
            "type": "u64"
          },
          {
            "name": "lastMarkPriceTwapTs",
            "docs": [
              "The last unix_timestamp the mark twap was updated."
            ],
            "type": "i64"
          },
          {
            "name": "lastBidPriceTwap",
            "docs": [
              "Average estimate of bid price over funding_period.",
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "lastAskPriceTwap",
            "docs": [
              "Average estimate of ask price over funding_period.",
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "markStd",
            "docs": [
              "Estimate of standard deviation of fill (mark) prices.",
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "oracleStd",
            "docs": [
              "Estimate of standard deviation of the oracle price at each update.",
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "lastOracleConfPct",
            "docs": [
              "The pct size of the oracle confidence interval.",
              "precision: PERCENTAGE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "volume24h",
            "docs": [
              "Estimated total of volume in market.",
              "QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "longIntensityVolume",
            "docs": [
              "The volume intensity of long fills (across all makers)."
            ],
            "type": "u64"
          },
          {
            "name": "shortIntensityVolume",
            "docs": [
              "The volume intensity of short fills (across all makers)."
            ],
            "type": "u64"
          },
          {
            "name": "lastTradeTs",
            "docs": [
              "The blockchain unix_timestamp at the time of the last trade."
            ],
            "type": "i64"
          },
          {
            "name": "last24hAvgFundingRate",
            "docs": [
              "estimate of last 24h of funding rate perp market (unit is quote per base)",
              "Market-wide config / rolling stat — read by the AMM when computing",
              "`reference_price_offset` and by funding-rate updates. Migrated from",
              "`PerpMarket` so the AMM reads only from `MarketStats`.",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "fundingPeriod",
            "docs": [
              "the periodicity of the funding rate updates. Market-wide config used",
              "across the funding path. Migrated from `PerpMarket`."
            ],
            "type": "i64"
          },
          {
            "name": "minOrderSize",
            "docs": [
              "the minimum base size of an order. Market-wide config read by the AMM",
              "when computing fallback prices / spread reserves. Migrated from",
              "`PerpMarket`.",
              "precision: BASE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "mmOraclePrice",
            "docs": [
              "MM oracle price snapshot (set by the native handler)."
            ],
            "type": "i64"
          },
          {
            "name": "mmOracleSlot",
            "docs": [
              "Slot at which the mm_oracle_* fields were last updated."
            ],
            "type": "u64"
          },
          {
            "name": "mmOracleSequenceId",
            "docs": [
              "Monotonically increasing sequence id for mm_oracle updates."
            ],
            "type": "u64"
          },
          {
            "name": "lastOracleNormalisedPrice",
            "docs": [
              "Canonical sanitised/clamped oracle price — the latest oracle reading",
              "after normalisation (any quoter's view, not AMM-specific)."
            ],
            "type": "i64"
          },
          {
            "name": "lastReferencePriceOffset",
            "docs": [
              "Previous reference price offset, written by `_update_amm` after a",
              "successful repeg/k_update. Read by `update_amm_quote_state` to",
              "implement the legacy time-decayed reference-price-offset smoothing",
              "transition — when the freshly computed offset's sign flips relative",
              "to this cached value AND `curve_update_intensity > 100`, the",
              "transition is clamped per-slot rather than snapping. Migrated from",
              "`AMM.reference_price_offset` (which was deleted in the AMM-decoupling",
              "refactor) so the smoothing behaviour is preserved across cranks.",
              "precision: PRICE_PRECISION"
            ],
            "type": "i32"
          },
          {
            "name": "lastOracleValid",
            "docs": [
              "Whether the oracle was valid at the most recent `_update_amm`.",
              "Read by settlement and fill paths to gate operations."
            ],
            "type": "bool"
          },
          {
            "name": "padding",
            "docs": [
              "Padding so last_funding_oracle_twap is 8-aligned."
            ],
            "type": {
              "array": [
                "u8",
                3
              ]
            }
          },
          {
            "name": "lastFundingOracleTwap",
            "docs": [
              "Oracle TWAP captured at last funding update, the normalizer",
              "`last_24h_avg_funding_rate` accrued against. Read by the AMM's",
              "funding bias spread and `get_last_funding_basis`. Migrated from",
              "`PerpMarket` so the AMM reads only from `MarketStats`.",
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "historicalOracleData",
            "docs": [
              "Historical oracle readings — TWAPs, last raw price, confidence, delay,",
              "timestamp. Market-wide data (any quoter would want it), updated by",
              "`_update_amm` / funding paths. Migrated from AMM."
            ],
            "type": {
              "defined": {
                "name": "historicalOracleData"
              }
            }
          }
        ]
      }
    },
    {
      "name": "marketStatus",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "initialized"
          },
          {
            "name": "active"
          },
          {
            "name": "reduceOnly"
          },
          {
            "name": "settlement"
          },
          {
            "name": "delisted"
          }
        ]
      }
    },
    {
      "name": "marketType",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "spot"
          },
          {
            "name": "perp"
          }
        ]
      }
    },
    {
      "name": "modifyClobOrderParams",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "orderRef",
            "docs": [
              "Handle for the order being modified; the CLOB fails closed on a stale",
              "hint, and velocity fails the whole call if the removal hit anyone else."
            ],
            "type": {
              "defined": {
                "name": "clobOrderRefV0"
              }
            }
          },
          {
            "name": "price",
            "docs": [
              "`None` keeps the resting price."
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "baseAssetAmount",
            "docs": [
              "`None` keeps the *remaining* size of the resting order (not its",
              "original size)."
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "maxTs",
            "docs": [
              "`None` keeps the resting expiry (read off the book node before the",
              "cancel — the CLOB's removal response doesn't carry it). `Some(0)` makes",
              "the replacement good-till-cancelled."
            ],
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "activationDelaySlots",
            "docs": [
              "Same rule as `place_clob_order`: `None` takes the book's default speed",
              "bump, anything below it needs the flow-authority attestation."
            ],
            "type": {
              "option": "u32"
            }
          },
          {
            "name": "rejectIfCrossed",
            "docs": [
              "Same rule as `place_clob_order`: refuse the replacement rather than",
              "rest it crossed. The original is already off the book when this fires,",
              "so a refused replacement leaves the maker with no order — which is what",
              "a maker repricing into a crossed book is asking for."
            ],
            "type": "bool"
          }
        ]
      }
    },
    {
      "name": "modifyOrderParams",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "direction",
            "type": {
              "option": {
                "defined": {
                  "name": "positionDirection"
                }
              }
            }
          },
          {
            "name": "baseAssetAmount",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "price",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "reduceOnly",
            "type": {
              "option": "bool"
            }
          },
          {
            "name": "postOnly",
            "type": {
              "option": {
                "defined": {
                  "name": "postOnlyParam"
                }
              }
            }
          },
          {
            "name": "bitFlags",
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "maxTs",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "triggerPrice",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "triggerCondition",
            "type": {
              "option": {
                "defined": {
                  "name": "orderTriggerCondition"
                }
              }
            }
          },
          {
            "name": "oraclePriceOffset",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "auctionDuration",
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "auctionStartPrice",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "auctionEndPrice",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "policy",
            "type": {
              "option": "u8"
            }
          }
        ]
      }
    },
    {
      "name": "newUserRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "docs": [
              "unix_timestamp of action"
            ],
            "type": "i64"
          },
          {
            "name": "userAuthority",
            "type": "pubkey"
          },
          {
            "name": "user",
            "type": "pubkey"
          },
          {
            "name": "subAccountId",
            "type": "u16"
          },
          {
            "name": "name",
            "type": {
              "array": [
                "u8",
                32
              ]
            }
          },
          {
            "name": "referrer",
            "type": "pubkey"
          }
        ]
      }
    },
    {
      "name": "oracleGuardRails",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "priceDivergence",
            "type": {
              "defined": {
                "name": "priceDivergenceGuardRails"
              }
            }
          },
          {
            "name": "validity",
            "type": {
              "defined": {
                "name": "validityGuardRails"
              }
            }
          }
        ]
      }
    },
    {
      "name": "oracleSource",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "pyth"
          },
          {
            "name": "deprecatedSwitchboard"
          },
          {
            "name": "quoteAsset"
          },
          {
            "name": "pyth1K"
          },
          {
            "name": "pyth1M"
          },
          {
            "name": "pythStableCoin"
          },
          {
            "name": "prelaunch"
          },
          {
            "name": "pythPull"
          },
          {
            "name": "pyth1KPull"
          },
          {
            "name": "pyth1MPull"
          },
          {
            "name": "pythStableCoinPull"
          },
          {
            "name": "deprecatedSwitchboardOnDemand"
          },
          {
            "name": "pythLazer"
          },
          {
            "name": "pythLazer1K"
          },
          {
            "name": "pythLazer1M"
          },
          {
            "name": "pythLazerStableCoin"
          }
        ]
      }
    },
    {
      "name": "order",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "slot",
            "docs": [
              "The slot the order was placed"
            ],
            "type": "u64"
          },
          {
            "name": "price",
            "docs": [
              "The limit price for the order (can be 0 for market orders)",
              "For orders with an auction, this price isn't used until the auction is complete",
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "baseAssetAmount",
            "docs": [
              "The size of the order",
              "precision for perps: BASE_PRECISION",
              "precision for spot: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "baseAssetAmountFilled",
            "docs": [
              "The amount of the order filled",
              "precision for perps: BASE_PRECISION",
              "precision for spot: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "quoteAssetAmountFilled",
            "docs": [
              "The amount of quote filled for the order",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "triggerPrice",
            "docs": [
              "At what price the order will be triggered. Only relevant for trigger orders",
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "auctionStartPrice",
            "docs": [
              "The start price for the auction. Only relevant for market/oracle orders",
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "auctionEndPrice",
            "docs": [
              "The end price for the auction. Only relevant for market/oracle orders",
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "maxTs",
            "docs": [
              "The time when the order will expire"
            ],
            "type": "i64"
          },
          {
            "name": "oraclePriceOffset",
            "docs": [
              "If set, the order limit price is the oracle price + this offset",
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "orderId",
            "docs": [
              "The id for the order. Each users has their own order id space"
            ],
            "type": "u32"
          },
          {
            "name": "marketIndex",
            "docs": [
              "The perp/spot market index"
            ],
            "type": "u16"
          },
          {
            "name": "status",
            "docs": [
              "Whether the order is open or unused"
            ],
            "type": {
              "defined": {
                "name": "orderStatus"
              }
            }
          },
          {
            "name": "orderType",
            "docs": [
              "The type of order"
            ],
            "type": {
              "defined": {
                "name": "orderType"
              }
            }
          },
          {
            "name": "marketType",
            "docs": [
              "Whether market is spot or perp"
            ],
            "type": {
              "defined": {
                "name": "marketType"
              }
            }
          },
          {
            "name": "userOrderId",
            "docs": [
              "User generated order id. Can make it easier to place/cancel orders"
            ],
            "type": "u8"
          },
          {
            "name": "existingPositionDirection",
            "docs": [
              "What the users position was when the order was placed"
            ],
            "type": {
              "defined": {
                "name": "positionDirection"
              }
            }
          },
          {
            "name": "direction",
            "docs": [
              "Whether the user is going long or short. LONG = bid, SHORT = ask"
            ],
            "type": {
              "defined": {
                "name": "positionDirection"
              }
            }
          },
          {
            "name": "reduceOnly",
            "docs": [
              "Whether the order is allowed to only reduce position size"
            ],
            "type": "bool"
          },
          {
            "name": "postOnly",
            "docs": [
              "Whether the order must be a maker"
            ],
            "type": "bool"
          },
          {
            "name": "immediateOrCancel",
            "docs": [
              "Whether the order must be canceled the same slot it is placed"
            ],
            "type": "bool"
          },
          {
            "name": "triggerCondition",
            "docs": [
              "Whether the order is triggered above or below the trigger price. Only relevant for trigger orders"
            ],
            "type": {
              "defined": {
                "name": "orderTriggerCondition"
              }
            }
          },
          {
            "name": "auctionDuration",
            "docs": [
              "How many slots the auction lasts"
            ],
            "type": "u8"
          },
          {
            "name": "postedSlotTail",
            "docs": [
              "Last 8 bits of the slot the order was posted on-chain (not order slot for signed msg orders)"
            ],
            "type": "u8"
          },
          {
            "name": "bitFlags",
            "docs": [
              "Bitflags for further classification",
              "0: is_signed_message"
            ],
            "type": "u8"
          },
          {
            "name": "routeDigest",
            "docs": [
              "The route this order's signer chose, as",
              "[`crate::state::order_params::route_digest`] of the `QuoterV0` entries",
              "their signed message named. Zero when no route was signed, which is",
              "every directly-placed order.",
              "",
              "A digest rather than the list because an `Order` has no room for",
              "pubkeys, and stored bytes here cost 32 slots each. The filler supplies",
              "the list and this pins which list it may supply — the check that the",
              "fill actually *carried* those entries is then a containment test",
              "against the transaction. Bytes, not a `u32`, to stay alignment-free in",
              "the middle of a byte run."
            ],
            "type": {
              "array": [
                "u8",
                4
              ]
            }
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                1
              ]
            }
          }
        ]
      }
    },
    {
      "name": "orderAction",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "place"
          },
          {
            "name": "cancel"
          },
          {
            "name": "fill"
          },
          {
            "name": "trigger"
          },
          {
            "name": "expire"
          }
        ]
      }
    },
    {
      "name": "orderActionExplanation",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "none"
          },
          {
            "name": "insufficientFreeCollateral"
          },
          {
            "name": "oraclePriceBreachedLimitPrice"
          },
          {
            "name": "marketOrderFilledToLimitPrice"
          },
          {
            "name": "orderExpired"
          },
          {
            "name": "liquidation"
          },
          {
            "name": "orderFilledWithAmm"
          },
          {
            "name": "orderFilledWithAmmJit"
          },
          {
            "name": "orderFilledWithMatch"
          },
          {
            "name": "orderFilledWithMatchJit"
          },
          {
            "name": "marketExpired"
          },
          {
            "name": "riskingIncreasingOrder"
          },
          {
            "name": "reduceOnlyOrderIncreasedPosition"
          },
          {
            "name": "orderFillWithSerum"
          },
          {
            "name": "noBorrowLiquidity"
          },
          {
            "name": "orderFillWithPhoenix"
          },
          {
            "name": "orderFilledWithAmmJitLpSplit"
          },
          {
            "name": "orderFilledWithLpJit"
          },
          {
            "name": "deriskLp"
          },
          {
            "name": "orderFilledWithOpenbookV2"
          },
          {
            "name": "transferPerpPosition"
          },
          {
            "name": "orderFilledWithExternalQuoter"
          },
          {
            "name": "clobOrderEvicted"
          },
          {
            "name": "clobRemainderCulled"
          }
        ]
      }
    },
    {
      "name": "orderActionRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "action",
            "type": {
              "defined": {
                "name": "orderAction"
              }
            }
          },
          {
            "name": "actionExplanation",
            "type": {
              "defined": {
                "name": "orderActionExplanation"
              }
            }
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "marketType",
            "type": {
              "defined": {
                "name": "marketType"
              }
            }
          },
          {
            "name": "filler",
            "type": {
              "option": "pubkey"
            }
          },
          {
            "name": "fillerReward",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "fillRecordId",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "baseAssetAmountFilled",
            "docs": [
              "precision: BASE_PRECISION (perp) or MINT_PRECISION (spot)"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "quoteAssetAmountFilled",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "takerFee",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "makerFee",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "referrerReward",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": {
              "option": "u32"
            }
          },
          {
            "name": "quoteAssetAmountSurplus",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "spotFulfillmentMethodFee",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "taker",
            "type": {
              "option": "pubkey"
            }
          },
          {
            "name": "takerOrderId",
            "type": {
              "option": "u32"
            }
          },
          {
            "name": "takerOrderDirection",
            "type": {
              "option": {
                "defined": {
                  "name": "positionDirection"
                }
              }
            }
          },
          {
            "name": "takerOrderBaseAssetAmount",
            "docs": [
              "precision: BASE_PRECISION (perp) or MINT_PRECISION (spot)"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "takerOrderCumulativeBaseAssetAmountFilled",
            "docs": [
              "precision: BASE_PRECISION (perp) or MINT_PRECISION (spot)"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "takerOrderCumulativeQuoteAssetAmountFilled",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "maker",
            "type": {
              "option": "pubkey"
            }
          },
          {
            "name": "makerOrderId",
            "type": {
              "option": "u32"
            }
          },
          {
            "name": "makerOrderDirection",
            "type": {
              "option": {
                "defined": {
                  "name": "positionDirection"
                }
              }
            }
          },
          {
            "name": "makerOrderBaseAssetAmount",
            "docs": [
              "precision: BASE_PRECISION (perp) or MINT_PRECISION (spot)"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "makerOrderCumulativeBaseAssetAmountFilled",
            "docs": [
              "precision: BASE_PRECISION (perp) or MINT_PRECISION (spot)"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "makerOrderCumulativeQuoteAssetAmountFilled",
            "docs": [
              "precision: QUOTE_PRECISION"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "oraclePrice",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "bitFlags",
            "docs": [
              "Order bit flags, defined in [`crate::state::user::OrderBitFlag`]"
            ],
            "type": "u8"
          },
          {
            "name": "takerExistingQuoteEntryAmount",
            "docs": [
              "precision: QUOTE_PRECISION",
              "Only Some if the taker reduced position"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "takerExistingBaseAssetAmount",
            "docs": [
              "precision: BASE_PRECISION",
              "Only Some if the taker flipped position direction"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "makerExistingQuoteEntryAmount",
            "docs": [
              "precision: QUOTE_PRECISION",
              "Only Some if the maker reduced position"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "makerExistingBaseAssetAmount",
            "docs": [
              "precision: BASE_PRECISION",
              "Only Some if the maker flipped position direction"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "triggerPrice",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "builderIdx",
            "docs": [
              "the idx of the builder in the taker's [`RevenueShareEscrow`] account"
            ],
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "builderFee",
            "docs": [
              "precision: QUOTE_PRECISION builder fee paid by the taker"
            ],
            "type": {
              "option": "u64"
            }
          }
        ]
      }
    },
    {
      "name": "orderFillerRewardStructure",
      "docs": [
        "`u128` is placed first so `#[repr(C)]` layout matches between host (x86_64,",
        "align 16 in Rust ≥ 1.77) and the SBF VM (align 8). Trailing `_padding`",
        "rounds the struct to a host-portable 32 bytes. See",
        "`docs/alignment-and-native-offsets.md`."
      ],
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "timeBasedRewardLowerBound",
            "type": "u128"
          },
          {
            "name": "rewardNumerator",
            "type": "u32"
          },
          {
            "name": "rewardDenominator",
            "type": "u32"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                8
              ]
            }
          }
        ]
      }
    },
    {
      "name": "orderParams",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "orderType",
            "type": {
              "defined": {
                "name": "orderType"
              }
            }
          },
          {
            "name": "marketType",
            "type": {
              "defined": {
                "name": "marketType"
              }
            }
          },
          {
            "name": "direction",
            "type": {
              "defined": {
                "name": "positionDirection"
              }
            }
          },
          {
            "name": "userOrderId",
            "type": "u8"
          },
          {
            "name": "baseAssetAmount",
            "type": "u64"
          },
          {
            "name": "price",
            "type": "u64"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "reduceOnly",
            "type": "bool"
          },
          {
            "name": "postOnly",
            "type": {
              "defined": {
                "name": "postOnlyParam"
              }
            }
          },
          {
            "name": "bitFlags",
            "type": "u8"
          },
          {
            "name": "maxTs",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "triggerPrice",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "triggerCondition",
            "type": {
              "defined": {
                "name": "orderTriggerCondition"
              }
            }
          },
          {
            "name": "oraclePriceOffset",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "auctionDuration",
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "auctionStartPrice",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "auctionEndPrice",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "builderIdx",
            "docs": [
              "the index into the placing user's RevenueShareEscrow.approved_builders list, if this order",
              "carries a builder code. Only honored for non-swift orders; swift orders carry the builder",
              "info in the signed message envelope instead."
            ],
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "builderFeeTenthBps",
            "docs": [
              "the builder fee on this order, in tenths of a bps, e.g. 100 = 0.01%"
            ],
            "type": {
              "option": "u16"
            }
          }
        ]
      }
    },
    {
      "name": "orderRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "user",
            "type": "pubkey"
          },
          {
            "name": "order",
            "type": {
              "defined": {
                "name": "order"
              }
            }
          }
        ]
      }
    },
    {
      "name": "orderStatus",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "init"
          },
          {
            "name": "open"
          },
          {
            "name": "filled"
          },
          {
            "name": "canceled"
          }
        ]
      }
    },
    {
      "name": "orderTriggerCondition",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "above"
          },
          {
            "name": "below"
          },
          {
            "name": "triggeredAbove"
          },
          {
            "name": "triggeredBelow"
          }
        ]
      }
    },
    {
      "name": "orderType",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "market"
          },
          {
            "name": "limit"
          },
          {
            "name": "triggerMarket"
          },
          {
            "name": "triggerLimit"
          },
          {
            "name": "oracle"
          }
        ]
      }
    },
    {
      "name": "overrideAmmCacheParams",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "quoteOwedFromLpPool",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "lastSettleSlot",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "lastFeePoolTokenAmount",
            "type": {
              "option": "u128"
            }
          },
          {
            "name": "lastNetPnlPoolTokenAmount",
            "type": {
              "option": "i128"
            }
          },
          {
            "name": "ammPositionScalar",
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "ammInventoryLimit",
            "type": {
              "option": "i64"
            }
          }
        ]
      }
    },
    {
      "name": "perpBankruptcyRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "pnl",
            "type": "i128"
          },
          {
            "name": "ifPayment",
            "type": "u128"
          },
          {
            "name": "clawbackUser",
            "type": {
              "option": "pubkey"
            }
          },
          {
            "name": "clawbackUserPayment",
            "type": {
              "option": "u128"
            }
          },
          {
            "name": "cumulativeFundingRateDelta",
            "type": "i128"
          }
        ]
      }
    },
    {
      "name": "perpMarket",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "pubkey",
            "docs": [
              "The perp market's address. It is a pda of the market index"
            ],
            "type": "pubkey"
          },
          {
            "name": "baseAssetAmountLong",
            "docs": [
              "always non-negative. tracks number of total longs in market (regardless of counterparty)",
              "precision: BASE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "baseAssetAmountShort",
            "docs": [
              "always non-positive. tracks number of total shorts in market (regardless of counterparty)",
              "precision: BASE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "quoteAssetAmount",
            "docs": [
              "sum of all user's perp quote_asset_amount in market",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "quoteEntryAmountLong",
            "docs": [
              "sum of all long user's quote_entry_amount in market",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "quoteEntryAmountShort",
            "docs": [
              "sum of all short user's quote_entry_amount in market",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "quoteBreakEvenAmountLong",
            "docs": [
              "sum of all long user's quote_break_even_amount in market",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "quoteBreakEvenAmountShort",
            "docs": [
              "sum of all short user's quote_break_even_amount in market",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i128"
          },
          {
            "name": "maxOpenInterest",
            "docs": [
              "max allowed open interest, blocks trades that breach this value",
              "precision: BASE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "totalSocialLoss",
            "docs": [
              "accumulated social loss paid by users since inception in market",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "cumulativeFundingRateLong",
            "docs": [
              "accumulated funding rate for longs since inception in market"
            ],
            "type": "i128"
          },
          {
            "name": "cumulativeFundingRateShort",
            "docs": [
              "accumulated funding rate for shorts since inception in market"
            ],
            "type": "i128"
          },
          {
            "name": "feeLedger",
            "docs": [
              "The market's fee ledger: every fee-split counter in one place (gross",
              "analytics, pending protocol/IF carveouts, and the AMM's backstop",
              "tranche). Mutate through its accessor methods, not raw field writes."
            ],
            "type": {
              "defined": {
                "name": "feeLedger"
              }
            }
          },
          {
            "name": "oracle",
            "docs": [
              "oracle price data public key"
            ],
            "type": "pubkey"
          },
          {
            "name": "pnlPool",
            "docs": [
              "The market's pnl pool. When users settle negative pnl, the balance increases.",
              "When users settle positive pnl, the balance decreases. Can not go negative."
            ],
            "type": {
              "defined": {
                "name": "poolBalance"
              }
            }
          },
          {
            "name": "protocolFeePool",
            "docs": [
              "Protocol fees collected on this perp market, quote/USDC-denominated — a",
              "protocol-owned Deposit-type claim against the quote spot market vault",
              "(like `pnl_pool`; counted in the quote market's `deposit_balance`).",
              "Owned by the protocol, not users, and never part of the insurance",
              "backstop. `market_index` is set to `quote_spot_market_index`. Withdrawn",
              "directly to `State.protocol_fee_recipient_perp`."
            ],
            "type": {
              "defined": {
                "name": "poolBalance"
              }
            }
          },
          {
            "name": "protocolLiquidationFee",
            "docs": [
              "Protocol's cut of a perp liquidation, taken from the liquidatee.",
              "precision: LIQUIDATOR_FEE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "takerFeeAddonTenthBps",
            "docs": [
              "Additive per-market taker-fee surcharge in tenth-bps (10 = 1bp),",
              "unsigned: surcharge only (e.g. toxic-flow markets), never a discount.",
              "A discount could push the taker fee below the maker rebate it must",
              "fund and revert every match fill; promo discounts go through",
              "`State.promo_fee_tier` instead. Applied on top of the tier fee before",
              "`fee_adjustment` scales the sum:",
              "`taker_fee = (tier_fee + add-on) * (1 +/- fee_adjustment%)`.",
              "Taker fee only; the maker rebate and the post-only path see",
              "`fee_adjustment` alone. Occupies 2 bytes of the former 4-byte",
              "`_padding_buffer` (same offset/alignment on all targets), so existing",
              "accounts read 0 = no add-on until the admin sets it."
            ],
            "type": "u16"
          },
          {
            "name": "paddingBuffer",
            "type": {
              "array": [
                "u8",
                2
              ]
            }
          },
          {
            "name": "feePoolBufferTarget",
            "docs": [
              "The pnl-pool retention buffer the streaming sweep's IF and",
              "AMM-provision drains leave untouched: `sweep_market_fees` drains",
              "those pendings only from what the pnl pool holds above",
              "`max(net_user_pnl, 0) + fee_pool_buffer_target`. The protocol drain",
              "is EXEMPT — it reserves only `max(net_user_pnl, 0)` and runs first;",
              "it sweeps every settle, so each drain stays small, and its pending is",
              "no bankruptcy tranche so retaining it buys nothing.",
              "",
              "Why a buffer on top of the user-claims reservation: `net_user_pnl`",
              "is a mark-to-market snapshot, so a pool swept to the exact mark is",
              "short on the next adverse oracle tick — and the sweep is a one-way",
              "valve, so the slack can't be cheaply recalled (IF value returns only",
              "through capped gated paths, the AMM provision only via bankruptcy",
              "clawback). The buffer throttles those outflows per sweep; pool tokens",
              "are fungible (pendings are counters, not segregated tokens), so",
              "whichever cut lingers keeps settling winners in the meantime. This",
              "delays materialization, it does not divert anyone's cut. Side",
              "benefits: an unswept IF cut gives THIS market uncapped market-local",
              "bankruptcy coverage (tranche 1) instead of capped shared-vault",
              "coverage, and the buffer damps the IF settle ratchet (value settled",
              "into the IF accrues to stakers permanently).",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "name",
            "docs": [
              "Encoded display name for the perp market e.g. SOL-PERP"
            ],
            "type": {
              "array": [
                "u8",
                32
              ]
            }
          },
          {
            "name": "insuranceClaim",
            "docs": [
              "The perp market's claim on the insurance fund"
            ],
            "type": {
              "defined": {
                "name": "insuranceClaim"
              }
            }
          },
          {
            "name": "lastFundingRate",
            "docs": [
              "last funding rate in this perp market (unit is quote per base)",
              "precision: FUNDING_RATE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "lastFundingRateLong",
            "docs": [
              "last funding rate for longs in this perp market (unit is quote per base)",
              "precision: FUNDING_RATE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "lastFundingRateShort",
            "docs": [
              "last funding rate for shorts in this perp market (unit is quote per base)",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "lastFundingRateTs",
            "docs": [
              "the last funding rate update unix_timestamp"
            ],
            "type": "i64"
          },
          {
            "name": "netUnsettledFundingPnl",
            "docs": [
              "unsettled funding pnl across the market (protocol-wide)"
            ],
            "type": "i64"
          },
          {
            "name": "fundingClampThreshold",
            "docs": [
              "dead-zone threshold for the funding premium. mark/oracle twap spreads",
              "within +/- this band are treated as noise and add no premium; spreads",
              "past it are shrunk toward zero by this amount so funding stays continuous",
              "across the boundary. fit per market post-launch",
              "precision: BPS_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "fundingRampSlope",
            "docs": [
              "slope of the funding premium ramp above the dead zone. 1.0x passes the",
              "shrunk spread through unchanged; higher leans into the premium harder",
              "fit per market post-launch",
              "precision: PERCENTAGE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "orderStepSize",
            "docs": [
              "the base step size (increment) of orders",
              "precision: BASE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "orderTickSize",
            "docs": [
              "the price tick size of orders",
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "unrealizedPnlMaxImbalance",
            "docs": [
              "The max pnl imbalance before positive pnl asset weight is discounted",
              "pnl imbalance is the difference between long and short pnl. When it's greater than 0,",
              "the amm has negative pnl and the initial asset weight for positive pnl is discounted",
              "precision = QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "expiryTs",
            "docs": [
              "The ts when the market will be expired. Only set if market is in reduce only mode"
            ],
            "type": "i64"
          },
          {
            "name": "expiryPrice",
            "docs": [
              "The price at which positions will be settled. Only set if market is expired",
              "precision = PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "nextFillRecordId",
            "docs": [
              "Every trade has a fill record id. This is the next id to be used"
            ],
            "type": "u64"
          },
          {
            "name": "nextFundingRateRecordId",
            "docs": [
              "Every funding rate update has a record id. This is the next id to be used"
            ],
            "type": "u64"
          },
          {
            "name": "imfFactor",
            "docs": [
              "The initial margin fraction factor. Used to increase margin ratio for large positions",
              "precision: MARGIN_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "unrealizedPnlImfFactor",
            "docs": [
              "The imf factor for unrealized pnl. Used to discount asset weight for large positive pnl",
              "precision: MARGIN_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "liquidatorFee",
            "docs": [
              "The fee the liquidator is paid for taking over perp position",
              "precision: LIQUIDATOR_FEE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "ifLiquidationFee",
            "docs": [
              "The fee the insurance fund receives from liquidation",
              "precision: LIQUIDATOR_FEE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "marginRatioInitial",
            "docs": [
              "The margin ratio which determines how much collateral is required to open a position",
              "e.g. margin ratio of .1 means a user must have $100 of total collateral to open a $1000 position",
              "precision: MARGIN_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "marginRatioMaintenance",
            "docs": [
              "The margin ratio which determines when a user will be liquidated",
              "e.g. margin ratio of .05 means a user must have $50 of total collateral to maintain a $1000 position",
              "else they will be liquidated",
              "precision: MARGIN_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "unrealizedPnlInitialAssetWeight",
            "docs": [
              "The initial asset weight for positive pnl. Negative pnl always has an asset weight of 1",
              "precision: SPOT_WEIGHT_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "unrealizedPnlMaintenanceAssetWeight",
            "docs": [
              "The maintenance asset weight for positive pnl. Negative pnl always has an asset weight of 1",
              "precision: SPOT_WEIGHT_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "numberOfUsersWithBase",
            "docs": [
              "number of users in a position (base)"
            ],
            "type": "u32"
          },
          {
            "name": "numberOfUsers",
            "docs": [
              "number of users in a position (pnl) or pnl (quote)"
            ],
            "type": "u32"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "status",
            "docs": [
              "Whether a market is active, reduce only, expired, etc",
              "Affects whether users can open/close positions"
            ],
            "type": {
              "defined": {
                "name": "marketStatus"
              }
            }
          },
          {
            "name": "contractType",
            "docs": [
              "Currently only Perpetual markets are supported"
            ],
            "type": {
              "defined": {
                "name": "contractType"
              }
            }
          },
          {
            "name": "contractTier",
            "docs": [
              "The contract tier determines how much insurance a market can receive, with more speculative markets receiving less insurance",
              "It also influences the order perp markets can be liquidated, with less speculative markets being liquidated first"
            ],
            "type": {
              "defined": {
                "name": "contractTier"
              }
            }
          },
          {
            "name": "pausedOperations",
            "type": "u8"
          },
          {
            "name": "quoteSpotMarketIndex",
            "docs": [
              "The spot market that pnl is settled in"
            ],
            "type": "u16"
          },
          {
            "name": "feeAdjustment",
            "docs": [
              "Between -100 and 100, represents what % to increase/decrease the fee by",
              "E.g. if this is -50 and the fee is 5bps, the new fee will be 2.5bps",
              "if this is 50 and the fee is 5bps, the new fee will be 7.5bps"
            ],
            "type": "i16"
          },
          {
            "name": "pendingBankruptcyClaims",
            "docs": [
              "Number of unresolved bankrupt quote debts booked against this market.",
              "A liquidation that latches a user bankrupt increments it. Both writers",
              "of `PerpPosition.quote_asset_amount` decrement it when that debt",
              "reaches zero: `update_quote_asset_amount` and",
              "`update_position_and_market`. The count tracks the debt, not the latch:",
              "an un-latched estate that still owes the market stays booked, because",
              "the debt still resolves through the bankruptcy waterfall.",
              "",
              "While it is above zero the fee sweep withholds the whole",
              "`pending_if_fee`, not just `get_bankruptcy_if_floor()` — the sweep is",
              "permissionless, so a caller could otherwise drain the first-loss",
              "tranche between the latch and the resolution and push the loss onto the",
              "shared insurance fund or into socialization. The freeze is independent",
              "of open interest and of `bankruptcy_if_floor_pct`, both of which can be",
              "zero exactly when a bankruptcy is pending.",
              "",
              "Occupies 2 of the 6 bytes the Rust compiler inserts to 8-align",
              "`last_fill_price`. The remaining 4 stay explicit padding, so every",
              "later byte offset and the account size are unchanged and existing",
              "accounts read 0 (no pending claim).",
              "",
              "`settle_expired_market_pools_to_revenue_pool` rejects while this count",
              "is above zero, because that instruction's final sweep bypasses the",
              "floor."
            ],
            "type": "u16"
          },
          {
            "name": "paddingAlignLfp",
            "docs": [
              "Explicit padding so the IDL records the 4 bytes the Rust compiler",
              "still inserts to 8-align `last_fill_price`. Without this the JS borsh",
              "decoder (which reads sequentially after the variable-span enum",
              "`status`) reads every field past `pending_bankruptcy_claims` 4 bytes",
              "early."
            ],
            "type": {
              "array": [
                "u8",
                4
              ]
            }
          },
          {
            "name": "lastFillPrice",
            "type": "u64"
          },
          {
            "name": "poolId",
            "type": "u8"
          },
          {
            "name": "paddingPmm",
            "type": {
              "array": [
                "u8",
                2
              ]
            }
          },
          {
            "name": "paddingHedge",
            "docs": [
              "Was `lp_fee_transfer_scalar`, `lp_status`, `lp_paused_operations`,",
              "`lp_exchange_fee_excluscion_scalar`, `lp_pool_id` (5×u8). Relocated into",
              "`hedge_config` at the tail; kept as reserved bytes so existing account",
              "byte offsets (and snapshots) are undisturbed."
            ],
            "type": {
              "array": [
                "u8",
                5
              ]
            }
          },
          {
            "name": "marketConfig",
            "type": "u8"
          },
          {
            "name": "oracleSource",
            "docs": [
              "the oracle provider information. used to decode/scale the oracle public key"
            ],
            "type": {
              "defined": {
                "name": "oracleSource"
              }
            }
          },
          {
            "name": "oracleSlotDelayOverride",
            "docs": [
              "Max oracle delay, in slots, tolerated by immediate (JIT / auction-skipping)",
              "AMM fills. Positive is an explicit threshold. `0` disables immediate AMM",
              "fills entirely. Negative (the init default, `-1`) means unset, which",
              "resolves by price source: `MM_ORACLE_MIN_SLOT_GAP` for an MM-oracle-sourced",
              "price (the tightest window the crank can satisfy, since the program refuses",
              "MM-oracle writes closer together than that) and `0` for an exchange-oracle",
              "price, which can be same-slot fresh. See `math::oracle::oracle_validity`."
            ],
            "type": "i8"
          },
          {
            "name": "oracleLowRiskSlotDelayOverride",
            "docs": [
              "the override for the state.min_perp_auction_duration",
              "0 is no override, -1 is disable speed bump, 1-100 is literal speed bump"
            ],
            "type": "i8"
          },
          {
            "name": "bankruptcyIfFloorPct",
            "docs": [
              "Floor on the unswept IF-fee carveout, as a percentage of open-interest",
              "notional (PERCENTAGE_PRECISION). The fee sweep's IF drain leaves",
              "`pending_if_fee` at (at least) this floor, so a standing first-loss",
              "tranche is available to `resolve_perp_bankruptcy` before any user is",
              "latched bankrupt — a permissionless sweep (or the inline sweep on any",
              "pnl settle) cannot drain the tranche below it. Notional is valued at",
              "the market's own oracle TWAP so a manipulated spot print can't crush",
              "the floor.",
              "",
              "`0` means `DEFAULT_BANKRUPTCY_IF_FLOOR_PCT`, so every market created",
              "before the field existed carries the standing tranche without an admin",
              "call. `BANKRUPTCY_IF_FLOOR_DISABLED` turns the floor off. Read it",
              "through `get_bankruptcy_if_floor_pct`, never directly.",
              "",
              "The floor sizes the tranche off market risk, which is a proxy for the",
              "loss and can be smaller than it. `pending_bankruptcy_claims` covers",
              "every latched bankruptcy exactly, by withholding all of",
              "`pending_if_fee` until it resolves.",
              "",
              "Occupies the former 4-byte trailing padding before `market_stats`",
              "(same offset/alignment on all targets)."
            ],
            "type": "u32"
          },
          {
            "name": "marketStats",
            "docs": [
              "Market-wide stats shared across all makers: mark/oracle TWAPs, std,",
              "volume, intensity, mm-oracle snapshot, `historical_oracle_data`,",
              "`last_oracle_normalised_price`, `last_oracle_valid`. Writers (e.g.",
              "`MarketStats::update_mark_std`, `update_volume_24h`, native",
              "`handle_update_mm_oracle_native`) update this directly."
            ],
            "type": {
              "defined": {
                "name": "marketStats"
              }
            }
          },
          {
            "name": "pendingRevenueShare",
            "docs": [
              "Aggregate accrued builder/referrer revenue-share owed out of this",
              "market's `pnl_pool` but not yet paid: incremented as builder and",
              "referrer fees accrue on fills (mirrors the per-order",
              "`RevenueShareOrder.fees_accrued` writes) and decremented as",
              "`sweep_completed_revenue_share_for_market` pays them. The",
              "permissionless fee sweep reserves it (like `max(net_user_pnl, 0)` and",
              "the floored IF tranche) so a protocol-fee drain can't move the tokens",
              "backing already-owed revenue share out of the pnl pool and leave those",
              "claims temporarily unpayable. precision: QUOTE_PRECISION.",
              "",
              "Occupies the 8 bytes Rust naturally inserts to 16-align AMM's leading",
              "u128 (formerly explicit `_padding_align_amm`): a u64 at the same",
              "8-aligned offset keeps every downstream byte offset and the total size",
              "unchanged, so legacy accounts read 0 (nothing owed) until fees accrue."
            ],
            "type": "u64"
          },
          {
            "name": "amm",
            "docs": [
              "The automated market maker. Last field so future quoter modules can",
              "land in the trailing region without disturbing earlier byte offsets",
              "— in the target architecture this account holds back-to-back",
              "per-quoter state slices (AMM, DLOB-maker state, future propAMM-style",
              "participants, …) and each module owns a contiguous span starting at",
              "a known offset."
            ],
            "type": {
              "defined": {
                "name": "amm"
              }
            }
          },
          {
            "name": "hedgeConfig",
            "docs": [
              "This market's hedge (LP pool) configuration. Sits immediately after `amm`",
              "so the trailing `[amm, hedge_config]` span is the contiguous VLP region."
            ],
            "type": {
              "defined": {
                "name": "hedgeConfig"
              }
            }
          },
          {
            "name": "clobQuoter",
            "docs": [
              "The market's canonical CLOB quoter registry entry (`QuoterV0` PDA).",
              "When set, every router fill must include it in its quoter section —",
              "the mandatory-baseline rule: a route can't exclude the public book.",
              "A dead entry (deactivated/unapproved) still has to be passed but is",
              "skipped at quote time, so killing the book never bricks fills.",
              "`Pubkey::default()` = no CLOB requirement."
            ],
            "type": "pubkey"
          }
        ]
      }
    },
    {
      "name": "perpMarketFeeSweepRecord",
      "docs": [
        "Emitted by the streaming fee sweep (`sweep_market_fees`) when it",
        "materializes pending fee carveouts out of a perp market's pnl pool."
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "docs": [
              "unix_timestamp of action"
            ],
            "type": "i64"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "ifSwept",
            "docs": [
              "pending insurance cut moved to the quote spot market's revenue_pool"
            ],
            "type": "u64"
          },
          {
            "name": "protocolSwept",
            "docs": [
              "pending protocol cut moved to the market's protocol_fee_pool"
            ],
            "type": "u64"
          },
          {
            "name": "ammProvisionTokenized",
            "docs": [
              "AMM fee provision tokenized into amm.fee_pool (booked at fill)"
            ],
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "perpPosition",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "lastCumulativeFundingRate",
            "docs": [
              "The perp market's last cumulative funding rate. Used to calculate the funding payment owed to user",
              "precision: FUNDING_RATE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "baseAssetAmount",
            "docs": [
              "the size of the users perp position",
              "precision: BASE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "quoteAssetAmount",
            "docs": [
              "Used to calculate the users pnl. Upon entry, is equal to base_asset_amount * avg entry price - fees",
              "Updated when the user open/closes position or settles pnl. Includes fees/funding",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "quoteBreakEvenAmount",
            "docs": [
              "The amount of quote the user would need to exit their position at to break even",
              "Updated when the user open/closes position or settles pnl. Includes fees/funding",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "quoteEntryAmount",
            "docs": [
              "The amount quote the user entered the position with. Equal to base asset amount * avg entry price",
              "Updated when the user open/closes position. Excludes fees/funding",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "openBids",
            "docs": [
              "The amount of non reduce only trigger orders the user has open",
              "precision: BASE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "openAsks",
            "docs": [
              "The amount of non reduce only trigger orders the user has open",
              "precision: BASE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "settledPnl",
            "docs": [
              "The amount of pnl settled in this market since opening the position",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "isolatedPositionScaledBalance",
            "docs": [
              "The scaled balance of the isolated position",
              "precision: SPOT_BALANCE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                2
              ]
            }
          },
          {
            "name": "maxMarginRatio",
            "type": "u16"
          },
          {
            "name": "marketIndex",
            "docs": [
              "The market index for the perp market"
            ],
            "type": "u16"
          },
          {
            "name": "openOrders",
            "docs": [
              "The number of open orders"
            ],
            "type": "u8"
          },
          {
            "name": "positionFlag",
            "type": "u8"
          }
        ]
      }
    },
    {
      "name": "placeClobOrderParams",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "direction",
            "docs": [
              "Long rests as a bid, Short as an ask."
            ],
            "type": {
              "defined": {
                "name": "positionDirection"
              }
            }
          },
          {
            "name": "price",
            "type": "u64"
          },
          {
            "name": "baseAssetAmount",
            "type": "u64"
          },
          {
            "name": "maxTs",
            "docs": [
              "0 = good-till-cancelled."
            ],
            "type": "i64"
          },
          {
            "name": "activationDelaySlots",
            "docs": [
              "None = the CLOB market's default speed bump. Anything below the",
              "default requires the flow-authority attestation (the transaction",
              "co-signed by `State.hot_flow_authority`, introspected off the",
              "instructions sysvar); the CLOB clamps to its max."
            ],
            "type": {
              "option": "u32"
            }
          },
          {
            "name": "rejectIfCrossed",
            "docs": [
              "Refuse the placement when the order would cross the opposite best",
              "price, rather than resting it crossed. What a post-only order asks for.",
              "",
              "It is not what makes the order a maker. A CLOB order always fills at",
              "its own price on the maker fee schedule — a router taker takes it",
              "there, and a crossed pair settles through the cross crank, which runs",
              "the protocol `User` as the taker on both legs. This is about the order",
              "resting at all: a maker that quotes through the other side has",
              "mispriced and would rather place nothing."
            ],
            "type": "bool"
          }
        ]
      }
    },
    {
      "name": "poolBalance",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "scaledBalance",
            "docs": [
              "To get the pool's token amount, you must multiply the scaled balance by the market's cumulative",
              "deposit interest",
              "precision: SPOT_BALANCE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "marketIndex",
            "docs": [
              "The spot market the pool is for"
            ],
            "type": "u16"
          },
          {
            "name": "padding",
            "docs": [
              "Filler for the alignment gap before the two dust fields. Those fields must",
              "start at offsets 20 and 24. The host layout and the SBF layout then agree,",
              "and the packed borsh layout in the IDL reaches the same offsets. This",
              "field shrank from 14 bytes to 2. The size of the struct and every other",
              "field offset are unchanged. Do not reorder or resize these fields."
            ],
            "type": {
              "array": [
                "u8",
                2
              ]
            }
          },
          {
            "name": "pendingInterestSplitDust",
            "docs": [
              "Remainder of one index-space division that splits a spot market's deposit",
              "interest between lenders and the carveout pools. The accrual carries the",
              "remainder between intervals. A share too small to reach a whole index unit",
              "is therefore delayed and not lost.",
              "",
              "The division depends on the pool. See `split_deposit_interest`.",
              "",
              "- On `revenue_pool` this is the lenders-vs-carveouts split. The divisor",
              "is IF_FACTOR_PRECISION, so the value stays below IF_FACTOR_PRECISION.",
              "- On `protocol_fee_pool` this is the insurance-fund-vs-protocol split of",
              "the withheld amount. The divisor is",
              "`if_fee_factor + protocol_fee_factor`, so the value stays below it. The",
              "admin can lower that pair, which leaves a stored value at or above the",
              "new divisor. `split_deposit_interest` reduces the value it reads below",
              "the divisor in force, so a change of the factors costs less than one",
              "index unit and cannot strand the market.",
              "",
              "That order keeps the two carveouts from taking more than the interval",
              "gain. The first division bounds the total. The second division only",
              "divides the amount that the first division set aside. Two independent cuts",
              "can instead each round up and leave lenders at zero.",
              "",
              "precision: the numerator units of its division."
            ],
            "type": "u32"
          },
          {
            "name": "pendingInterestDust",
            "docs": [
              "Remainder of the token-space division for this pool's carveout.",
              "",
              "A withheld index amount reaches the pool only as whole tokens, through",
              "`deposit_balance * cut / 10^(19 - decimals)`. On a small market that",
              "division floors to zero even when the index-space cut is not zero. Lenders",
              "have already given up the value at that point, so a floored cut credits",
              "nobody and leaves unattributed slack in the vault. The accrual parks the",
              "remainder here and adds it back on the next interval.",
              "",
              "precision: token * 10^(19 - decimals). The value always stays below one",
              "token, which is `10^(19 - decimals)` and at most 10^19. It therefore fits",
              "a u64 for every supported value of `decimals`.",
              "",
              "Only the two lending carveout pools use these two fields. Those pools are",
              "a spot market's `revenue_pool` and `protocol_fee_pool`. Both fields stay 0",
              "on every other `PoolBalance`, such as a perp market's `pnl_pool` and",
              "`fee_pool`. Both fields read 0 on markets created before the fields",
              "existed, which is the correct starting value."
            ],
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "positionDirection",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "long"
          },
          {
            "name": "short"
          }
        ]
      }
    },
    {
      "name": "postOnlyParam",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "none"
          },
          {
            "name": "mustPostOnly"
          },
          {
            "name": "tryPostOnly"
          },
          {
            "name": "slide"
          }
        ]
      }
    },
    {
      "name": "prelaunchOracle",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "price",
            "type": "i64"
          },
          {
            "name": "maxPrice",
            "type": "i64"
          },
          {
            "name": "confidence",
            "type": "u64"
          },
          {
            "name": "lastUpdateSlot",
            "type": "u64"
          },
          {
            "name": "ammLastUpdateSlot",
            "type": "u64"
          },
          {
            "name": "perpMarketIndex",
            "type": "u16"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                70
              ]
            }
          }
        ]
      }
    },
    {
      "name": "prelaunchOracleParams",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "perpMarketIndex",
            "type": "u16"
          },
          {
            "name": "price",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "maxPrice",
            "type": {
              "option": "i64"
            }
          }
        ]
      }
    },
    {
      "name": "priceDivergenceGuardRails",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "markOraclePercentDivergence",
            "type": "u64"
          },
          {
            "name": "oracleTwap5minPercentDivergence",
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "protocolFeeWithdrawRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "docs": [
              "unix_timestamp of action"
            ],
            "type": "i64"
          },
          {
            "name": "marketIndex",
            "docs": [
              "perp market index for a perp-fee withdrawal, else the spot market index"
            ],
            "type": "u16"
          },
          {
            "name": "isPerp",
            "docs": [
              "true if this withdrawal drained a perp market's protocol_fee_pool",
              "(sourced from the quote spot vault), false for a spot market withdrawal"
            ],
            "type": "bool"
          },
          {
            "name": "spotMarketIndex",
            "docs": [
              "the spot market the tokens were drawn from"
            ],
            "type": "u16"
          },
          {
            "name": "amount",
            "type": "u64"
          },
          {
            "name": "recipientTokenAccount",
            "type": "pubkey"
          }
        ]
      }
    },
    {
      "name": "protocolUserWithdrawRecordV0",
      "docs": [
        "Emitted when the hot fee-withdraw role drains accumulated crank rewards",
        "from the protocol-owned `User` (`withdraw_protocol_user_deposit`).",
        "",
        "Versioned in the name, unlike the records inherited from upstream. An",
        "`#[event]`'s discriminator is derived from its struct name, so adding a",
        "field to a `…Record` changes the payload under a discriminator consumers",
        "already decode — the old decoder either truncates or fails, and nothing on",
        "the wire says which shape it got. A field addition here ships as",
        "`ProtocolUserWithdrawRecordV1` instead, with its own discriminator, so an",
        "old subscriber ignores it rather than mis-parsing it. Events velocity adds",
        "from here on follow the same rule."
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "docs": [
              "unix_timestamp of action"
            ],
            "type": "i64"
          },
          {
            "name": "spotMarketIndex",
            "docs": [
              "the spot market the tokens were drawn from"
            ],
            "type": "u16"
          },
          {
            "name": "amount",
            "type": "u64"
          },
          {
            "name": "protocolUser",
            "docs": [
              "the protocol-owned `User` account debited"
            ],
            "type": "pubkey"
          },
          {
            "name": "recipientTokenAccount",
            "type": "pubkey"
          }
        ]
      }
    },
    {
      "name": "pythLazerOracle",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "price",
            "type": "i64"
          },
          {
            "name": "publishTime",
            "type": "u64"
          },
          {
            "name": "postedSlot",
            "type": "u64"
          },
          {
            "name": "exponent",
            "type": "i32"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                4
              ]
            }
          },
          {
            "name": "conf",
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "quoteRouterArgs",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "direction",
            "type": {
              "defined": {
                "name": "directionV0"
              }
            }
          },
          {
            "name": "size",
            "docs": [
              "Size to quote up to. The books returned are what's available for a",
              "taker of this size — resting sources are merely truncated by it, the",
              "vAMM and PropAMMs genuinely price against it."
            ],
            "type": "u64"
          },
          {
            "name": "quoterCount",
            "docs": [
              "`QuoterV0` entries at the head of the quoter section of",
              "`remaining_accounts`; the rest of that section is their CPI accounts."
            ],
            "type": "u8"
          },
          {
            "name": "includeVamm",
            "docs": [
              "Quote the vAMM into the buffer as well.",
              "",
              "A market with more quoters than one view can carry is read in several",
              "passes. The vAMM prices against every other book in the same call, so",
              "a pass holding a subset would shade it against a subset and each pass",
              "would return a different vAMM. Exactly one pass sets this, and the",
              "caller merges the vAMM from that one. The passes that clear it also",
              "stop paying to compute a ladder they would discard."
            ],
            "type": "bool"
          }
        ]
      }
    },
    {
      "name": "quotedLevelV0",
      "docs": [
        "A quoted level, in the buffer's Pod form (the wire `PriceLevel` is borsh)."
      ],
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "price",
            "type": "u64"
          },
          {
            "name": "size",
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "quotedRowV0",
      "docs": [
        "One resting order behind a quoted book, in the buffer's Pod form (the wire",
        "`L3RowV0` is the same bytes).",
        "",
        "A book's ladder aggregates orders that belong to different people, and a",
        "caller that has to carry those accounts — or draw the book — needs them",
        "apart. Every other quoter fills from the one account its registry entry",
        "names, so its rows say that instead, and a consumer reads one shape either",
        "way."
      ],
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "price",
            "type": "u64"
          },
          {
            "name": "size",
            "type": "u64"
          },
          {
            "name": "orderId",
            "docs": [
              "The quoter's own handle for the order. Zero when the row is not an",
              "order but a rung attributed to the quoter's user."
            ],
            "type": "u64"
          },
          {
            "name": "authority",
            "docs": [
              "Authority of the `User` this row settles against."
            ],
            "type": "pubkey"
          },
          {
            "name": "subAccountId",
            "type": "u16"
          },
          {
            "name": "flags",
            "docs": [
              "`L3_ROW_FLAG_*`, as the quoter reported them."
            ],
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                5
              ]
            }
          }
        ]
      }
    },
    {
      "name": "quotedSourceKind",
      "docs": [
        "Which kind of liquidity a quoted book came from. The router needs this to",
        "know how to *execute* the allocation (a CPI leg, an in-program DLOB order,",
        "or the vAMM), and a UI needs it to label depth honestly — a PropAMM's",
        "levels are a quote at a size, not resting orders."
      ],
      "repr": {
        "kind": "rust"
      },
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "vamm"
          },
          {
            "name": "dlobOrder"
          },
          {
            "name": "quoter"
          }
        ]
      }
    },
    {
      "name": "quotedSourceV0",
      "docs": [
        "One source's slice of the buffer's level region."
      ],
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "key",
            "docs": [
              "The `QuoterV0` entry for a `Quoter`, the maker's `User` for a",
              "`DlobOrder`, the perp market for `Vamm`."
            ],
            "type": "pubkey"
          },
          {
            "name": "levelCount",
            "docs": [
              "Live levels in this source's slot of `levels`."
            ],
            "type": "u16"
          },
          {
            "name": "priority",
            "docs": [
              "Routing tier the split will apply (`QuoterV0::priority`, or the",
              "type default for the in-program sources)."
            ],
            "type": "u8"
          },
          {
            "name": "kind",
            "type": {
              "defined": {
                "name": "quotedSourceKind"
              }
            }
          },
          {
            "name": "clamped",
            "docs": [
              "Set when verification reduced this book — a Custom quoter advertising",
              "more depth than its `User`'s margin supports gets truncated here, so",
              "the caller never routes against or displays phantom depth. The",
              "difference between what the quoter said and what came back."
            ],
            "type": "bool"
          },
          {
            "name": "rowStart",
            "docs": [
              "This source's slice of `rows`: where it starts and how long it is. A",
              "source with no per-order detail has none."
            ],
            "type": "u8"
          },
          {
            "name": "rowLen",
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                1
              ]
            }
          }
        ]
      }
    },
    {
      "name": "quoterAccountMetaArg",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "pubkey",
            "type": "pubkey"
          },
          {
            "name": "isWritable",
            "type": "bool"
          }
        ]
      }
    },
    {
      "name": "quoterCpiLeg",
      "docs": [
        "Which CPI leg an account-list update targets."
      ],
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "quote"
          },
          {
            "name": "execute"
          }
        ]
      }
    },
    {
      "name": "quoterCrossConditionsV0",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "relay",
            "docs": [
              "Everything relay needs hosted, in one field: the `relay-spec` header,",
              "the condition slots, and the resolver account list (written at attach)",
              "every condition here points at. First field, so its watch offset",
              "is 8."
            ],
            "type": {
              "defined": {
                "name": "relayBlock3x40",
                "generics": [
                  {
                    "kind": "const",
                    "value": "3"
                  },
                  {
                    "kind": "const",
                    "value": "40"
                  }
                ]
              }
            }
          },
          {
            "name": "quoter",
            "docs": [
              "The Custom entry these conditions discover crosses for."
            ],
            "type": "pubkey"
          },
          {
            "name": "clobQuoter",
            "docs": [
              "The market's canonical CLOB entry / book / program, captured at",
              "attach time (the resolver stages the executor's CLOB leg from here",
              "without holding those accounts). Re-attach after a CLOB rotation."
            ],
            "type": "pubkey"
          },
          {
            "name": "clobMarket",
            "type": "pubkey"
          },
          {
            "name": "clobProgram",
            "type": "pubkey"
          },
          {
            "name": "oracle",
            "docs": [
              "The market's oracle, captured at attach time (the staged executor's",
              "map section)."
            ],
            "type": "pubkey"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "quoteSpotMarketIndex",
            "type": "u16"
          },
          {
            "name": "padding",
            "docs": [
              "Tail reserve: 4 bytes of alignment slack plus room for two more",
              "captured pubkeys, so a resolver that needs another fixed account can",
              "take it from here instead of forcing an `extend_account` migration on",
              "every attached quoter entry."
            ],
            "type": {
              "array": [
                "u8",
                68
              ]
            }
          }
        ]
      }
    },
    {
      "name": "quoterType",
      "repr": {
        "kind": "rust"
      },
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "vamm"
          },
          {
            "name": "clob"
          },
          {
            "name": "custom"
          }
        ]
      }
    },
    {
      "name": "quoterV0",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "user",
            "docs": [
              "For Custom quoters, the User this quoter is allowed to quote for.",
              "That user's authority creates the entry, so creation is consent. For",
              "vAMM, the vAMM user. For CLOB, ignored: execute may return balance",
              "changes for any user with resting orders on the CLOB."
            ],
            "type": "pubkey"
          },
          {
            "name": "programId",
            "docs": [
              "The external program invoked for `quote_v0` / `execute_v0`."
            ],
            "type": "pubkey"
          },
          {
            "name": "responseAccount",
            "docs": [
              "Account owned by `program_id` that quote/execute responses are written",
              "into; must be registered in both account lists. Responses are read at",
              "the pointer returned via return data, so payloads aren't bound by the",
              "1024-byte return-data cap.",
              "",
              "For CLOB entries this is the book itself — the CLOB's response region",
              "lives in its market account — which is what lets velocity read the",
              "resting orders an execute may touch without a second registered",
              "account to trust. Callers that depend on that re-derive the book's",
              "market index from its bytes rather than assume it."
            ],
            "type": "pubkey"
          },
          {
            "name": "authority",
            "docs": [
              "Manages this registry entry. For Custom quoters this is the quoted",
              "user's authority (enforced at creation, no handoff), so the maker can",
              "always kill their own quoter (`is_active`); the admin vets the CPI",
              "surface (`is_approved`), which any config change resets."
            ],
            "type": "pubkey"
          },
          {
            "name": "quoteV0Discriminator",
            "docs": [
              "Raw instruction discriminators on `program_id`. Stored rather than",
              "derived so non-Anchor programs can participate."
            ],
            "type": {
              "array": [
                "u8",
                8
              ]
            }
          },
          {
            "name": "executeV0Discriminator",
            "type": {
              "array": [
                "u8",
                8
              ]
            }
          },
          {
            "name": "quoteL3V0Discriminator",
            "docs": [
              "The optional third leg: `quote_l3_v0`, which reports the resting",
              "orders behind a ladder and who each belongs to. Zero means the quoter",
              "does not implement it, and a reader attributes the whole ladder to",
              "[`Self::user`] — which is right for every quoter that fills from one",
              "account. A book is the exception, and this is how it says so."
            ],
            "type": {
              "array": [
                "u8",
                8
              ]
            }
          },
          {
            "name": "quoteAccounts",
            "docs": [
              "Accounts forwarded to `quote_v0`, in order. Only the first",
              "`quote_accounts_count` entries are live."
            ],
            "type": {
              "array": [
                {
                  "defined": {
                    "name": "ammAccountMeta"
                  }
                },
                32
              ]
            }
          },
          {
            "name": "executeAccounts",
            "docs": [
              "Accounts forwarded to `execute_v0`, in order. Only the first",
              "`execute_accounts_count` entries are live."
            ],
            "type": {
              "array": [
                {
                  "defined": {
                    "name": "ammAccountMeta"
                  }
                },
                32
              ]
            }
          },
          {
            "name": "market",
            "docs": [
              "Perp market index this quoter serves."
            ],
            "type": "u16"
          },
          {
            "name": "quoterType",
            "type": {
              "defined": {
                "name": "quoterType"
              }
            }
          },
          {
            "name": "isActive",
            "docs": [
              "The authority's own on/off switch — always settable by the maker."
            ],
            "type": "bool"
          },
          {
            "name": "isApproved",
            "docs": [
              "Admin vetting of the CPI surface; reset by any config change."
            ],
            "type": "bool"
          },
          {
            "name": "priority",
            "docs": [
              "Routing priority: at a price, lower-priority tiers fill first, pro",
              "rata within a tier. Defaults by type (vAMM 0, CLOB 10, Custom 20);",
              "admin-set thereafter — never by the maker."
            ],
            "type": "u8"
          },
          {
            "name": "quoteAccountsCount",
            "type": "u8"
          },
          {
            "name": "executeAccountsCount",
            "type": "u8"
          },
          {
            "name": "watchOffset",
            "docs": [
              "Maker-declared reprice region: the account bytes whose change means",
              "\"this quoter may quote differently now\" (a midpoint's mid region, a",
              "custom AMM's parameter block). Relay cross-discovery conditions wake",
              "on it; `watch_len == 0` means no declaration (poll-only discovery).",
              "Config like everything else here: vetted by the admin via the",
              "`is_approved` reset — a watch that misses reprices only costs the",
              "maker cross latency, never correctness (the poll is the floor)."
            ],
            "type": "u32"
          },
          {
            "name": "watchLen",
            "type": "u32"
          },
          {
            "name": "watchAccount",
            "type": "pubkey"
          },
          {
            "name": "padding",
            "docs": [
              "Room for the next field, so adding one does not move the account's",
              "size or its alignment invariant."
            ],
            "type": {
              "array": [
                "u8",
                8
              ]
            }
          }
        ]
      }
    },
    {
      "name": "referrerName",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "authority",
            "type": "pubkey"
          },
          {
            "name": "user",
            "type": "pubkey"
          },
          {
            "name": "userStats",
            "type": "pubkey"
          },
          {
            "name": "name",
            "type": {
              "array": [
                "u8",
                32
              ]
            }
          }
        ]
      }
    },
    {
      "name": "relayBlock11x32",
      "docs": [
        "relay condition block (spec v0), 11 conditions, as one opaque wire region"
      ],
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "bytes",
            "type": {
              "array": [
                "u8",
                3200
              ]
            }
          }
        ]
      }
    },
    {
      "name": "relayBlock2x8",
      "docs": [
        "relay condition block (spec v0), 2 conditions, as one opaque wire region"
      ],
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "bytes",
            "type": {
              "array": [
                "u8",
                680
              ]
            }
          }
        ]
      }
    },
    {
      "name": "relayBlock3x40",
      "docs": [
        "relay condition block (spec v0), 3 conditions, as one opaque wire region"
      ],
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "bytes",
            "type": {
              "array": [
                "u8",
                1928
              ]
            }
          }
        ]
      }
    },
    {
      "name": "relayScratchV0",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "scratch",
            "type": {
              "array": [
                "u8",
                4096
              ]
            }
          }
        ]
      }
    },
    {
      "name": "revenueShare",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "authority",
            "docs": [
              "the owner of this account, a builder or referrer"
            ],
            "type": "pubkey"
          },
          {
            "name": "totalReferrerRewards",
            "type": "u64"
          },
          {
            "name": "totalBuilderRewards",
            "type": "u64"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                24
              ]
            }
          }
        ]
      }
    },
    {
      "name": "revenueShareEscrow",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "authority",
            "docs": [
              "the owner of this account, a user"
            ],
            "type": "pubkey"
          },
          {
            "name": "referrer",
            "type": "pubkey"
          },
          {
            "name": "reservedFixed",
            "type": {
              "array": [
                "u8",
                24
              ]
            }
          },
          {
            "name": "padding0",
            "type": "u32"
          },
          {
            "name": "orders",
            "type": {
              "vec": {
                "defined": {
                  "name": "revenueShareOrder"
                }
              }
            }
          },
          {
            "name": "padding1",
            "type": "u32"
          },
          {
            "name": "approvedBuilders",
            "type": {
              "vec": {
                "defined": {
                  "name": "builderInfo"
                }
              }
            }
          }
        ]
      }
    },
    {
      "name": "revenueShareOrder",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "feesAccrued",
            "docs": [
              "fees accrued so far for this order slot. This is not exclusively fees from this order_id",
              "and may include fees from other orders in the same market. This may be swept to the",
              "builder's SpotPosition during settle_pnl."
            ],
            "type": "u64"
          },
          {
            "name": "orderId",
            "docs": [
              "the order_id of the current active order in this slot. It's only relevant while bit_flag = Open"
            ],
            "type": "u32"
          },
          {
            "name": "feeTenthBps",
            "docs": [
              "the builder fee on this order, in tenths of a bps, e.g. 100 = 0.01%"
            ],
            "type": "u16"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "subAccountId",
            "docs": [
              "the subaccount_id of the user who created this order. It's only relevant while bit_flag = Open"
            ],
            "type": "u16"
          },
          {
            "name": "builderIdx",
            "docs": [
              "the index of the RevenueShareEscrow.approved_builders list, that this order's fee will settle to. Ignored",
              "if bit_flag = Referral."
            ],
            "type": "u8"
          },
          {
            "name": "bitFlags",
            "docs": [
              "bitflags that describe the state of the order.",
              "[`RevenueShareOrderBitFlag::Init`]: this order slot is available for use.",
              "[`RevenueShareOrderBitFlag::Open`]: this order slot is occupied, `order_id` is the `sub_account_id`'s active order.",
              "[`RevenueShareOrderBitFlag::Completed`]: this order has been filled or canceled, and is waiting to be settled into.",
              "the builder's account order_id and sub_account_id are no longer relevant, it may be merged with other orders.",
              "[`RevenueShareOrderBitFlag::Referral`]: this order stores referral rewards waiting to be settled for this market.",
              "If it is set, no other bitflag should be set."
            ],
            "type": "u8"
          },
          {
            "name": "userOrderIndex",
            "docs": [
              "the index into the User's orders list when this RevenueShareOrder was created, make sure to verify that order_id matches."
            ],
            "type": "u8"
          },
          {
            "name": "marketType",
            "type": {
              "defined": {
                "name": "marketType"
              }
            }
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                10
              ]
            }
          }
        ]
      }
    },
    {
      "name": "revenueShareSettleRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "builder",
            "type": {
              "option": "pubkey"
            }
          },
          {
            "name": "referrer",
            "type": {
              "option": "pubkey"
            }
          },
          {
            "name": "feeSettled",
            "type": "u64"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "marketType",
            "type": {
              "defined": {
                "name": "marketType"
              }
            }
          },
          {
            "name": "builderSubAccountId",
            "type": "u16"
          },
          {
            "name": "builderTotalReferrerRewards",
            "type": "u64"
          },
          {
            "name": "builderTotalBuilderRewards",
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "routerQuoteBufferV0",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "authority",
            "docs": [
              "Only this signer may quote into the buffer, so two routers sharing a",
              "market don't overwrite each other's reads."
            ],
            "type": "pubkey"
          },
          {
            "name": "quotedSize",
            "docs": [
              "Taker size the books were quoted at. Meaningful output, not an echo:",
              "resting books (CLOB, DLOB) are size-independent and merely truncated",
              "by it, while the vAMM's and a PropAMM's levels genuinely depend on it."
            ],
            "type": "u64"
          },
          {
            "name": "slot",
            "docs": [
              "Slot the quote ran at, so a cached book's staleness is checkable."
            ],
            "type": "u64"
          },
          {
            "name": "market",
            "type": "u16"
          },
          {
            "name": "sourceCount",
            "docs": [
              "Live entries in `sources`."
            ],
            "type": "u8"
          },
          {
            "name": "rowCount",
            "docs": [
              "Live entries in `rows`."
            ],
            "type": "u8"
          },
          {
            "name": "rowsTruncated",
            "docs": [
              "The rows region filled before every source had been described, so the",
              "last sources carry fewer rows than their books hold. The ladders are",
              "unaffected — a row is detail about a level, never the level itself."
            ],
            "type": "bool"
          },
          {
            "name": "direction",
            "docs": [
              "Taker direction quoted (`Direction` as u8: 0 = long, 1 = short)."
            ],
            "type": "u8"
          },
          {
            "name": "padding",
            "docs": [
              "Pads the header to 128 bytes: 12 bytes of alignment slack (so the",
              "struct stays a multiple of 16 and `(SIZE - 8) % 16 == 0` holds — see",
              "docs/alignment-and-native-offsets.md) plus room for two more pubkeys,",
              "so naming another account in the header doesn't shift `sources` /",
              "`levels` and break every off-chain decoder of this buffer."
            ],
            "type": {
              "array": [
                "u8",
                74
              ]
            }
          },
          {
            "name": "sources",
            "type": {
              "array": [
                {
                  "defined": {
                    "name": "quotedSourceV0"
                  }
                },
                16
              ]
            }
          },
          {
            "name": "levels",
            "docs": [
              "One slot per source, parallel to `sources`."
            ],
            "type": {
              "array": [
                {
                  "array": [
                    {
                      "defined": {
                        "name": "quotedLevelV0"
                      }
                    },
                    128
                  ]
                },
                16
              ]
            }
          },
          {
            "name": "rows",
            "docs": [
              "The orders behind the ladders, in the order the sources were quoted.",
              "Each source names its own run through `row_start`/`row_len`."
            ],
            "type": {
              "array": [
                {
                  "defined": {
                    "name": "quotedRowV0"
                  }
                },
                128
              ]
            }
          }
        ]
      }
    },
    {
      "name": "scaleOrderParams",
      "docs": [
        "Parameters for placing scale orders - multiple limit orders distributed across a price range"
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "marketType",
            "type": {
              "defined": {
                "name": "marketType"
              }
            }
          },
          {
            "name": "direction",
            "type": {
              "defined": {
                "name": "positionDirection"
              }
            }
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "totalBaseAssetAmount",
            "docs": [
              "Total base asset amount to distribute across all orders"
            ],
            "type": "u64"
          },
          {
            "name": "startPrice",
            "docs": [
              "Starting price for the scale (in PRICE_PRECISION)"
            ],
            "type": "u64"
          },
          {
            "name": "endPrice",
            "docs": [
              "Ending price for the scale (in PRICE_PRECISION)"
            ],
            "type": "u64"
          },
          {
            "name": "orderCount",
            "docs": [
              "Number of orders to place (min 2, max 32)"
            ],
            "type": "u8"
          },
          {
            "name": "sizeDistribution",
            "docs": [
              "How to distribute sizes across orders"
            ],
            "type": {
              "defined": {
                "name": "sizeDistribution"
              }
            }
          },
          {
            "name": "reduceOnly",
            "docs": [
              "Whether orders should be reduce-only"
            ],
            "type": "bool"
          },
          {
            "name": "postOnly",
            "docs": [
              "Post-only setting for all orders"
            ],
            "type": {
              "defined": {
                "name": "postOnlyParam"
              }
            }
          },
          {
            "name": "bitFlags",
            "docs": [
              "Order bit flags"
            ],
            "type": "u8"
          },
          {
            "name": "maxTs",
            "docs": [
              "Maximum timestamp for orders to be valid"
            ],
            "type": {
              "option": "i64"
            }
          }
        ]
      }
    },
    {
      "name": "settlePnlExplanation",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "none"
          },
          {
            "name": "expiredPosition"
          }
        ]
      }
    },
    {
      "name": "settlePnlMode",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "mustSettle"
          },
          {
            "name": "trySettle"
          }
        ]
      }
    },
    {
      "name": "settlePnlRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "user",
            "type": "pubkey"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "pnl",
            "type": "i128"
          },
          {
            "name": "baseAssetAmount",
            "type": "i64"
          },
          {
            "name": "quoteAssetAmountAfter",
            "type": "i64"
          },
          {
            "name": "quoteEntryAmount",
            "type": "i64"
          },
          {
            "name": "settlePrice",
            "type": "i64"
          },
          {
            "name": "explanation",
            "type": {
              "defined": {
                "name": "settlePnlExplanation"
              }
            }
          }
        ]
      }
    },
    {
      "name": "sideV0",
      "docs": [
        "Which side an order rests on: a bid makes its owner long, an ask short."
      ],
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "bid"
          },
          {
            "name": "ask"
          }
        ]
      }
    },
    {
      "name": "signedMsgOrderId",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "uuid",
            "type": {
              "array": [
                "u8",
                8
              ]
            }
          },
          {
            "name": "maxSlot",
            "type": "u64"
          },
          {
            "name": "orderId",
            "type": "u32"
          },
          {
            "name": "padding",
            "type": "u32"
          }
        ]
      }
    },
    {
      "name": "signedMsgOrderParamsDelegateMessage",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "signedMsgOrderParams",
            "type": {
              "defined": {
                "name": "orderParams"
              }
            }
          },
          {
            "name": "takerPubkey",
            "type": "pubkey"
          },
          {
            "name": "slot",
            "type": "u64"
          },
          {
            "name": "uuid",
            "type": {
              "array": [
                "u8",
                8
              ]
            }
          },
          {
            "name": "takeProfitOrderParams",
            "type": {
              "option": {
                "defined": {
                  "name": "signedMsgTriggerOrderParams"
                }
              }
            }
          },
          {
            "name": "stopLossOrderParams",
            "type": {
              "option": {
                "defined": {
                  "name": "signedMsgTriggerOrderParams"
                }
              }
            }
          },
          {
            "name": "maxMarginRatio",
            "type": {
              "option": "u16"
            }
          },
          {
            "name": "builderIdx",
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "builderFeeTenthBps",
            "type": {
              "option": "u16"
            }
          },
          {
            "name": "isolatedPositionDeposit",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "network",
            "docs": [
              "See [`SignedMsgOrderParamsMessage::network`]."
            ],
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "route",
            "docs": [
              "See [`SignedMsgOrderParamsMessage::route`]."
            ],
            "type": {
              "option": {
                "vec": "pubkey"
              }
            }
          }
        ]
      }
    },
    {
      "name": "signedMsgOrderParamsMessage",
      "docs": [
        "Trailing fields are appended, never inserted: the verifier zero-pads a",
        "short payload before decoding, so an older producer's message reads as",
        "`None` for everything it did not send (see",
        "`validation::sig_verification`)."
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "signedMsgOrderParams",
            "type": {
              "defined": {
                "name": "orderParams"
              }
            }
          },
          {
            "name": "subAccountId",
            "type": "u16"
          },
          {
            "name": "slot",
            "type": "u64"
          },
          {
            "name": "uuid",
            "type": {
              "array": [
                "u8",
                8
              ]
            }
          },
          {
            "name": "takeProfitOrderParams",
            "type": {
              "option": {
                "defined": {
                  "name": "signedMsgTriggerOrderParams"
                }
              }
            }
          },
          {
            "name": "stopLossOrderParams",
            "type": {
              "option": {
                "defined": {
                  "name": "signedMsgTriggerOrderParams"
                }
              }
            }
          },
          {
            "name": "maxMarginRatio",
            "type": {
              "option": "u16"
            }
          },
          {
            "name": "builderIdx",
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "builderFeeTenthBps",
            "type": {
              "option": "u16"
            }
          },
          {
            "name": "isolatedPositionDeposit",
            "type": {
              "option": "u64"
            }
          },
          {
            "name": "network",
            "docs": [
              "[`SIGNED_MSG_NETWORK_MAINNET`] / [`SIGNED_MSG_NETWORK_DEVNET`]."
            ],
            "type": {
              "option": "u8"
            }
          },
          {
            "name": "route",
            "docs": [
              "The route the taker signed for: `QuoterV0` entries of the **custom**",
              "quoters (PropAMMs) it wants used. The CLOB and the vAMM are the",
              "mandatory baseline of every router fill, so they are implicit and",
              "never named here. Advisory to the program today — swift forwards it",
              "to keepers, which is what makes a routed order reach the quoters the",
              "taker chose."
            ],
            "type": {
              "option": {
                "vec": "pubkey"
              }
            }
          }
        ]
      }
    },
    {
      "name": "signedMsgOrderRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "user",
            "type": "pubkey"
          },
          {
            "name": "hash",
            "type": "string"
          },
          {
            "name": "matchingOrderParams",
            "type": {
              "defined": {
                "name": "orderParams"
              }
            }
          },
          {
            "name": "userOrderId",
            "type": "u32"
          },
          {
            "name": "signedMsgOrderMaxSlot",
            "type": "u64"
          },
          {
            "name": "signedMsgOrderUuid",
            "type": {
              "array": [
                "u8",
                8
              ]
            }
          },
          {
            "name": "ts",
            "type": "i64"
          }
        ]
      }
    },
    {
      "name": "signedMsgTriggerOrderParams",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "triggerPrice",
            "type": "u64"
          },
          {
            "name": "baseAssetAmount",
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "signedMsgUserOrders",
      "docs": [
        "* This struct is a duplicate of SignedMsgUserOrdersZeroCopy\n * It is used to give anchor an struct to generate the idl for clients\n * The struct SignedMsgUserOrdersZeroCopy is used to load the data in efficiently"
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "authorityPubkey",
            "type": "pubkey"
          },
          {
            "name": "padding",
            "type": "u32"
          },
          {
            "name": "signedMsgOrderData",
            "type": {
              "vec": {
                "defined": {
                  "name": "signedMsgOrderId"
                }
              }
            }
          }
        ]
      }
    },
    {
      "name": "signedMsgWsDelegates",
      "docs": [
        "* Used to store authenticated delegates for swift-like ws connections"
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "delegates",
            "type": {
              "vec": "pubkey"
            }
          }
        ]
      }
    },
    {
      "name": "sizeDistribution",
      "docs": [
        "How to distribute order sizes across scale orders"
      ],
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "flat"
          },
          {
            "name": "ascending"
          },
          {
            "name": "descending"
          }
        ]
      }
    },
    {
      "name": "spotBalanceType",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "deposit"
          },
          {
            "name": "borrow"
          }
        ]
      }
    },
    {
      "name": "spotBankruptcyRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "borrowAmount",
            "type": "u128"
          },
          {
            "name": "ifPayment",
            "type": "u128"
          },
          {
            "name": "cumulativeDepositInterestDelta",
            "type": "u128"
          }
        ]
      }
    },
    {
      "name": "spotInterestRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "depositBalance",
            "docs": [
              "precision: SPOT_BALANCE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "cumulativeDepositInterest",
            "docs": [
              "precision: SPOT_CUMULATIVE_INTEREST_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "borrowBalance",
            "docs": [
              "precision: SPOT_BALANCE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "cumulativeBorrowInterest",
            "docs": [
              "precision: SPOT_CUMULATIVE_INTEREST_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "optimalUtilization",
            "docs": [
              "precision: PERCENTAGE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "optimalBorrowRate",
            "docs": [
              "precision: PERCENTAGE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "maxBorrowRate",
            "docs": [
              "precision: PERCENTAGE_PRECISION"
            ],
            "type": "u32"
          }
        ]
      }
    },
    {
      "name": "spotMarket",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "pubkey",
            "docs": [
              "The address of the spot market. It is a pda of the market index"
            ],
            "type": "pubkey"
          },
          {
            "name": "oracle",
            "docs": [
              "The oracle used to price the markets deposits/borrows"
            ],
            "type": "pubkey"
          },
          {
            "name": "mint",
            "docs": [
              "The token mint of the market"
            ],
            "type": "pubkey"
          },
          {
            "name": "vault",
            "docs": [
              "The vault used to store the market's deposits",
              "The amount in the vault should be equal to or greater than deposits - borrows"
            ],
            "type": "pubkey"
          },
          {
            "name": "name",
            "docs": [
              "The encoded display name for the market e.g. SOL"
            ],
            "type": {
              "array": [
                "u8",
                32
              ]
            }
          },
          {
            "name": "insuranceFund",
            "docs": [
              "Details on the insurance fund covering bankruptcies in this markets token",
              "Covers bankruptcies for borrows with this markets token and perps settling in this markets token"
            ],
            "type": {
              "defined": {
                "name": "insuranceFund"
              }
            }
          },
          {
            "name": "totalSpotFee",
            "docs": [
              "The total spot fees collected for this market",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "depositBalance",
            "docs": [
              "The sum of the scaled balances for deposits across users and pool balances",
              "To convert to the deposit token amount, multiply by the cumulative deposit interest",
              "precision: SPOT_BALANCE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "borrowBalance",
            "docs": [
              "The sum of the scaled balances for borrows across users and pool balances",
              "To convert to the borrow token amount, multiply by the cumulative borrow interest",
              "precision: SPOT_BALANCE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "cumulativeDepositInterest",
            "docs": [
              "The cumulative interest earned by depositors",
              "Used to calculate the deposit token amount from the deposit balance",
              "precision: SPOT_CUMULATIVE_INTEREST_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "cumulativeBorrowInterest",
            "docs": [
              "The cumulative interest earned by borrowers",
              "Used to calculate the borrow token amount from the borrow balance",
              "precision: SPOT_CUMULATIVE_INTEREST_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "totalSocialLoss",
            "docs": [
              "The total socialized loss from borrows, in the mint's token",
              "precision: token mint precision"
            ],
            "type": "u128"
          },
          {
            "name": "totalQuoteSocialLoss",
            "docs": [
              "The total socialized loss from borrows, in the quote market's token",
              "preicision: QUOTE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "revenuePool",
            "docs": [
              "Revenue the protocol has collected in this markets token",
              "e.g. for SOL-PERP, funds can be settled in usdc and will flow into the USDC revenue pool"
            ],
            "type": {
              "defined": {
                "name": "poolBalance"
              }
            }
          },
          {
            "name": "spotFeePool",
            "docs": [
              "The fees collected from swaps between this market and the quote market",
              "Is settled to the quote markets revenue pool"
            ],
            "type": {
              "defined": {
                "name": "poolBalance"
              }
            }
          },
          {
            "name": "historicalOracleData",
            "type": {
              "defined": {
                "name": "historicalOracleData"
              }
            }
          },
          {
            "name": "historicalIndexData",
            "type": {
              "defined": {
                "name": "historicalIndexData"
              }
            }
          },
          {
            "name": "withdrawGuardThreshold",
            "docs": [
              "no withdraw limits/guards when deposits below this threshold",
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "maxTokenDeposits",
            "docs": [
              "The max amount of token deposits in this market",
              "0 if there is no limit",
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "depositTokenTwap",
            "docs": [
              "24hr average of deposit token amount",
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "borrowTokenTwap",
            "docs": [
              "24hr average of borrow token amount",
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "utilizationTwap",
            "docs": [
              "24hr average of utilization",
              "which is borrow amount over token amount",
              "precision: SPOT_UTILIZATION_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "lastInterestTs",
            "docs": [
              "Last time the cumulative deposit and borrow interest was updated"
            ],
            "type": "u64"
          },
          {
            "name": "lastTwapTs",
            "docs": [
              "Last time the deposit/borrow/utilization averages were updated"
            ],
            "type": "u64"
          },
          {
            "name": "expiryTs",
            "docs": [
              "The time the market is set to expire. Only set if market is in reduce only mode"
            ],
            "type": "i64"
          },
          {
            "name": "orderStepSize",
            "docs": [
              "Spot orders must be a multiple of the step size",
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "orderTickSize",
            "docs": [
              "Spot orders must be a multiple of the tick size",
              "precision: PRICE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "minOrderSize",
            "docs": [
              "The minimum order size",
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "maxPositionSize",
            "docs": [
              "The maximum spot position size",
              "if the limit is 0, there is no limit",
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "nextFillRecordId",
            "docs": [
              "Every spot trade has a fill record id. This is the next id to use"
            ],
            "type": "u64"
          },
          {
            "name": "nextDepositRecordId",
            "docs": [
              "Every deposit has a deposit record id. This is the next id to use"
            ],
            "type": "u64"
          },
          {
            "name": "initialAssetWeight",
            "docs": [
              "The initial asset weight used to calculate a deposits contribution to a users initial total collateral",
              "e.g. if the asset weight is .8, $100 of deposits contributes $80 to the users initial total collateral",
              "precision: SPOT_WEIGHT_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "maintenanceAssetWeight",
            "docs": [
              "The maintenance asset weight used to calculate a deposits contribution to a users maintenance total collateral",
              "e.g. if the asset weight is .9, $100 of deposits contributes $90 to the users maintenance total collateral",
              "precision: SPOT_WEIGHT_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "initialLiabilityWeight",
            "docs": [
              "The initial liability weight used to calculate a borrows contribution to a users initial margin requirement",
              "e.g. if the liability weight is .9, $100 of borrows contributes $90 to the users initial margin requirement",
              "precision: SPOT_WEIGHT_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "maintenanceLiabilityWeight",
            "docs": [
              "The maintenance liability weight used to calculate a borrows contribution to a users maintenance margin requirement",
              "e.g. if the liability weight is .8, $100 of borrows contributes $80 to the users maintenance margin requirement",
              "precision: SPOT_WEIGHT_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "imfFactor",
            "docs": [
              "The initial margin fraction factor. Used to increase liability weight/decrease asset weight for large positions",
              "precision: MARGIN_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "liquidatorFee",
            "docs": [
              "The fee the liquidator is paid for taking over borrow/deposit",
              "precision: LIQUIDATOR_FEE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "ifLiquidationFee",
            "docs": [
              "The fee the insurance fund receives from liquidation",
              "precision: LIQUIDATOR_FEE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "optimalUtilization",
            "docs": [
              "The optimal utilization rate for this market.",
              "Used to determine the markets borrow rate",
              "precision: SPOT_UTILIZATION_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "optimalBorrowRate",
            "docs": [
              "The borrow rate for this market when the market has optimal utilization",
              "precision: SPOT_RATE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "maxBorrowRate",
            "docs": [
              "The borrow rate for this market when the market has 1000 utilization",
              "precision: SPOT_RATE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "decimals",
            "docs": [
              "The market's token mint's decimals. To from decimals to a precision, 10^decimals"
            ],
            "type": "u32"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "ordersEnabled",
            "docs": [
              "Whether or not spot trading is enabled"
            ],
            "type": "bool"
          },
          {
            "name": "oracleSource",
            "type": {
              "defined": {
                "name": "oracleSource"
              }
            }
          },
          {
            "name": "status",
            "type": {
              "defined": {
                "name": "marketStatus"
              }
            }
          },
          {
            "name": "assetTier",
            "docs": [
              "The asset tier affects how a deposit can be used as collateral and the priority for a borrow being liquidated"
            ],
            "type": {
              "defined": {
                "name": "assetTier"
              }
            }
          },
          {
            "name": "pausedOperations",
            "type": "u8"
          },
          {
            "name": "ifPausedOperations",
            "type": "u8"
          },
          {
            "name": "feeAdjustment",
            "type": "i16"
          },
          {
            "name": "maxTokenBorrowsFraction",
            "docs": [
              "What fraction of max_token_deposits",
              "disabled when 0, 1 => 1/10000 => .01% of max_token_deposits",
              "precision: X/10000"
            ],
            "type": "u16"
          },
          {
            "name": "flashLoanAmount",
            "docs": [
              "For swaps, the amount of token loaned out in the begin_swap ix",
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "flashLoanInitialTokenAmount",
            "docs": [
              "For swaps, the amount in the users token account in the begin_swap ix",
              "Used to calculate how much of the token left the system in end_swap ix",
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "totalSwapFee",
            "docs": [
              "The total fees received from swaps",
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "scaleInitialAssetWeightStart",
            "docs": [
              "When to begin scaling down the initial asset weight",
              "disabled when 0",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "minBorrowRate",
            "docs": [
              "The min borrow rate for this market when the market regardless of utilization",
              "1 => 1/200 => .5%",
              "precision: X/200"
            ],
            "type": "u8"
          },
          {
            "name": "tokenProgramFlag",
            "type": "u8"
          },
          {
            "name": "poolId",
            "type": "u8"
          },
          {
            "name": "paddingAlignPfp",
            "docs": [
              "Explicit filler carved from the alignment gap before `protocol_fee_pool`",
              "(which must stay at struct offset 752 so host/SBF layouts agree and the",
              "borsh/IDL packed offset matches). The gap is 13 bytes; the three",
              "configurable-limit fields below plus this 1-byte filler fill it exactly,",
              "so `protocol_fee_pool` and every field after it keep their offsets and the",
              "account size is unchanged. Reads 0 on markets created before these fields",
              "existed. Every byte is explicit so no implicit `#[repr(C)]` pad desyncs",
              "off-chain borsh decoders. Do not reorder or resize."
            ],
            "type": "u8"
          },
          {
            "name": "withdrawCircuitBreakerBps",
            "docs": [
              "Daily withdraw circuit-breaker size: the max fraction of the 24h deposit",
              "TWAP that may be withdrawn per 24h window. `0` is treated as the default",
              "(2500 bps = 25%) so markets created before this field existed keep prior",
              "behavior. precision: basis points (10_000 = 100%)"
            ],
            "type": "u16"
          },
          {
            "name": "maxDepositBpsPerDay",
            "docs": [
              "Daily deposit rate limit: the max fraction above the 24h deposit TWAP that",
              "resulting deposits may reach per 24h window. Disabled when `0`.",
              "precision: basis points (10_000 = 100%)"
            ],
            "type": "u16"
          },
          {
            "name": "depositGuardThreshold",
            "docs": [
              "No deposit rate limit when resulting deposits are below this threshold.",
              "Mirrors `withdraw_guard_threshold` on the deposit side.",
              "precision: token mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "protocolFeePool",
            "docs": [
              "Protocol fees collected in this market's token (lending protocol carveout",
              "+ spot-liquidation protocol fee). A protocol-owned Deposit-type claim",
              "inside the spot vault (counted in `deposit_balance`, like `revenue_pool`)",
              "— owned by the protocol, not users, and never part of the insurance",
              "backstop. Withdrawn directly to `State.protocol_fee_recipient_spot`; the",
              "withdrawal decrements this claim and re-validates the vault still covers",
              "all remaining claims, so it can never tap user deposits."
            ],
            "type": {
              "defined": {
                "name": "poolBalance"
              }
            }
          },
          {
            "name": "protocolLiquidationFee",
            "docs": [
              "Protocol's cut of a spot liquidation, taken from the liquidatee.",
              "precision: LIQUIDATOR_FEE_PRECISION"
            ],
            "type": "u32"
          },
          {
            "name": "protocolFeeFactor",
            "docs": [
              "Protocol's carveout of lending deposit-interest gains, routed to",
              "`protocol_fee_pool`. precision: IF_FACTOR_PRECISION. A cut too small to",
              "reach a whole unit is carried on the carveout pools, not floored away. See",
              "`split_deposit_interest`."
            ],
            "type": "u32"
          },
          {
            "name": "ifLastSettleVaultAmount",
            "docs": [
              "Donation-proof accounted balance of the insurance-fund vault. It is moved",
              "by the same signed delta as the real SPL vault on *every* instruction that",
              "moves the vault, so it stays a faithful shadow of the vault minus raw",
              "donations. Inflows grow it: staker deposits (`add_insurance_fund_stake`)",
              "and settled revenue (`settle_revenue_to_insurance_fund`). Outflows/draws",
              "shrink it (saturating at 0): staker withdrawals",
              "(`remove_insurance_fund_stake`) and every IF draw that covers a loss —",
              "`resolve_perp_pnl_deficit`, `resolve_perp_bankruptcy`,",
              "`resolve_spot_bankruptcy`. The one movement deliberately *excluded* is a",
              "raw SPL transfer straight into the vault: it runs no instruction, so it",
              "never enters this balance — that is exactly the donation the shadow must",
              "not see. Consumed by the per-period revenue-settle APR cap in",
              "`settle_revenue_to_insurance_fund`, sized off `min(live_if_vault, this)`,",
              "so a donation spiked into the live vault right before a settle cannot",
              "inflate the cap while legitimate stakes and real settled revenue (which",
              "this balance tracks) still do. (The unstake-cancel share forfeiture is",
              "donation-proofed differently — by withdraw-and-restake at the active share",
              "price — and does *not* read this field.) Repurposed from trailing padding —",
              "layout/size unchanged; `0` means \"uninitialized\" (existing account",
              "pre-upgrade, or an accounted balance legitimately drained to empty — an",
              "empty IF vault has no user shares, so this is safe), and is seeded from the",
              "live balance on the next add/settle and treated as \"fall back to live\" by",
              "the consumers."
            ],
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "spotMarketVaultDepositRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "depositBalance",
            "docs": [
              "precision: SPOT_BALANCE_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "cumulativeDepositInterestBefore",
            "docs": [
              "precision: SPOT_CUMULATIVE_INTEREST_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "cumulativeDepositInterestAfter",
            "docs": [
              "precision: SPOT_CUMULATIVE_INTEREST_PRECISION"
            ],
            "type": "u128"
          },
          {
            "name": "depositTokenAmountBefore",
            "type": "u64"
          },
          {
            "name": "amount",
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "spotPosition",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "scaledBalance",
            "docs": [
              "The scaled balance of the position. To get the token amount, multiply by the cumulative deposit/borrow",
              "interest of corresponding market.",
              "precision: SPOT_BALANCE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "openBids",
            "docs": [
              "How many spot non reduce only trigger orders the user has open",
              "precision: token mint precision"
            ],
            "type": "i64"
          },
          {
            "name": "openAsks",
            "docs": [
              "How many spot non reduce only trigger orders the user has open",
              "precision: token mint precision"
            ],
            "type": "i64"
          },
          {
            "name": "cumulativeDeposits",
            "docs": [
              "The cumulative deposits/borrows a user has made into a market",
              "precision: token mint precision"
            ],
            "type": "i64"
          },
          {
            "name": "marketIndex",
            "docs": [
              "The market index of the corresponding spot market"
            ],
            "type": "u16"
          },
          {
            "name": "balanceType",
            "docs": [
              "Whether the position is deposit or borrow"
            ],
            "type": {
              "defined": {
                "name": "spotBalanceType"
              }
            }
          },
          {
            "name": "openOrders",
            "docs": [
              "Number of open orders"
            ],
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                4
              ]
            }
          }
        ]
      }
    },
    {
      "name": "stakeAction",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "stake"
          },
          {
            "name": "unstakeRequest"
          },
          {
            "name": "unstakeCancelRequest"
          },
          {
            "name": "unstake"
          },
          {
            "name": "unstakeTransfer"
          },
          {
            "name": "stakeTransfer"
          },
          {
            "name": "adminDeposit"
          }
        ]
      }
    },
    {
      "name": "state",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "coldAdmin",
            "docs": [
              "Root authority. Set at `initialize`; only this key can rotate `warm_admin`",
              "and `pause_admin`. Expected to sit behind a (small) timelocked multisig."
            ],
            "type": "pubkey"
          },
          {
            "name": "warmAdmin",
            "docs": [
              "Operational authority (e.g. multisig+timelock). Can rotate the 10 hot keys",
              "below. `Pubkey::default()` means unset — only `cold_admin` can act in that case."
            ],
            "type": "pubkey"
          },
          {
            "name": "pauseAdmin",
            "docs": [
              "Emergency-pause authority. No on-chain timelock — intended to live behind a",
              "fast-acting multisig that can flip pause flags without delay. May only *add*",
              "pause bits (never clear them); cold/warm retain full pause + unpause power.",
              "`Pubkey::default()` means unassigned (only cold/warm can pause)."
            ],
            "type": "pubkey"
          },
          {
            "name": "hotAmmCrank",
            "docs": [
              "Purpose-specific bot keys. `Pubkey::default()` means the role is unassigned",
              "and only warm/cold can call handlers gated on that role."
            ],
            "type": "pubkey"
          },
          {
            "name": "hotLpCache",
            "type": "pubkey"
          },
          {
            "name": "hotLpSwap",
            "type": "pubkey"
          },
          {
            "name": "hotLpSettle",
            "type": "pubkey"
          },
          {
            "name": "hotFeatureFlag",
            "type": "pubkey"
          },
          {
            "name": "hotFuel",
            "type": "pubkey"
          },
          {
            "name": "hotUserFlag",
            "type": "pubkey"
          },
          {
            "name": "hotVaultDeposit",
            "type": "pubkey"
          },
          {
            "name": "hotMmOracleCrank",
            "type": "pubkey"
          },
          {
            "name": "hotAmmSpreadAdjust",
            "type": "pubkey"
          },
          {
            "name": "whitelistMint",
            "type": "pubkey"
          },
          {
            "name": "discountMint",
            "type": "pubkey"
          },
          {
            "name": "signer",
            "type": "pubkey"
          },
          {
            "name": "srmVault",
            "type": "pubkey"
          },
          {
            "name": "perpFeeStructure",
            "type": {
              "defined": {
                "name": "feeStructure"
              }
            }
          },
          {
            "name": "spotFeeStructure",
            "type": {
              "defined": {
                "name": "feeStructure"
              }
            }
          },
          {
            "name": "oracleGuardRails",
            "type": {
              "defined": {
                "name": "oracleGuardRails"
              }
            }
          },
          {
            "name": "numberOfAuthorities",
            "type": "u64"
          },
          {
            "name": "numberOfSubAccounts",
            "type": "u64"
          },
          {
            "name": "liquidationMarginBufferRatio",
            "type": "u32"
          },
          {
            "name": "settlementDuration",
            "type": "u16"
          },
          {
            "name": "numberOfMarkets",
            "type": "u16"
          },
          {
            "name": "numberOfSpotMarkets",
            "type": "u16"
          },
          {
            "name": "signerNonce",
            "type": "u8"
          },
          {
            "name": "minPerpAuctionDuration",
            "type": "u8"
          },
          {
            "name": "defaultMarketOrderTimeInForce",
            "type": "u8"
          },
          {
            "name": "defaultSpotAuctionDuration",
            "type": "u8"
          },
          {
            "name": "exchangeStatus",
            "type": "u8"
          },
          {
            "name": "liquidationDuration",
            "type": "u8"
          },
          {
            "name": "initialPctToLiquidate",
            "type": "u16"
          },
          {
            "name": "maxNumberOfSubAccounts",
            "type": "u16"
          },
          {
            "name": "maxInitializeUserFee",
            "type": "u16"
          },
          {
            "name": "featureBitFlags",
            "type": "u8"
          },
          {
            "name": "lpPoolFeatureBitFlags",
            "type": "u8"
          },
          {
            "name": "solvencyStatus",
            "docs": [
              "Bitmask of `SolvencyStatus` flags. Gates internal solvency-repair flows",
              "(bankruptcy / pnl-deficit resolution) independently of `WithdrawPaused`,",
              "so user withdrawals can be halted while repair keeps running, or repair",
              "can be frozen on its own when an oracle is suspect. `0` = repair allowed."
            ],
            "type": "u8"
          },
          {
            "name": "protocolFeeRecipientPerp",
            "docs": [
              "Treasury that PERP protocol fees (quote-denominated) may be withdrawn",
              "to. Settable only by `cold_admin`. `withdraw_protocol_fees_perp` pays",
              "this key's associated token account (recipient-locked).",
              "`Pubkey::default()` (unset) makes perp withdrawals inert."
            ],
            "type": "pubkey"
          },
          {
            "name": "protocolFeeRecipientSpot",
            "docs": [
              "Treasury that SPOT protocol fees (each market's own token: lending",
              "carveouts + spot-liquidation cuts) may be withdrawn to. Settable only",
              "by `cold_admin`. `withdraw_protocol_fees_spot` pays this key's",
              "associated token account for the market's mint (recipient-locked).",
              "`Pubkey::default()` (unset) makes spot withdrawals inert."
            ],
            "type": "pubkey"
          },
          {
            "name": "hotFeeWithdraw",
            "docs": [
              "Hot key authorized for the `FeeWithdraw` role (triggers protocol-fee",
              "withdrawals to the configured recipients)."
            ],
            "type": "pubkey"
          },
          {
            "name": "hotAccountExtension",
            "docs": [
              "Hot key authorized for the `AccountExtension` role (grows zero-copy",
              "accounts to the deployed program's size after a struct-extending",
              "upgrade)."
            ],
            "type": "pubkey"
          },
          {
            "name": "promoFeeTier",
            "docs": [
              "Promotional fee-tier floor applied to every account: the effective",
              "perp fee tier is `max(volume tier, promo_fee_tier)` (clamped to the",
              "configured tier count), so nobody is downgraded by it. 0 = no-op",
              "(disabled), also what pre-upgrade accounts read from former padding.",
              "Reset to 0 and every account is back on its volume tier at its next",
              "fill; no per-user state."
            ],
            "type": "u8"
          },
          {
            "name": "hotFlowAuthority",
            "docs": [
              "The retail-flow attestation key (swift's). Not a signer of any admin",
              "instruction: transactions *co-signed* by this key are attested flow —",
              "`place_clob_order` accepts a faster-than-default activation delay",
              "only when instructions-sysvar introspection finds it among the",
              "transaction's signers, and quoters (e.g. the midpoint) apply their",
              "own equivalent check. `Pubkey::default()` (unset) disables fast",
              "activation entirely rather than leaving it open."
            ],
            "type": "pubkey"
          },
          {
            "name": "padding0",
            "docs": [
              "Alignment slack ahead of `transaction_fee_rails`, taken out of the",
              "former padding: the tail's offset is odd by two and the rails hold",
              "`u32`s."
            ],
            "type": {
              "array": [
                "u8",
                2
              ]
            }
          },
          {
            "name": "transactionFeeRails",
            "docs": [
              "What one transaction costs the account that sends it, as the network",
              "prices it now. Every relay crank payment is derived from this, so a",
              "change to the network's fee model is one write here instead of a",
              "re-price of every market."
            ],
            "type": {
              "defined": {
                "name": "transactionFeeRails"
              }
            }
          },
          {
            "name": "liquidationCrankReimbursementBps",
            "docs": [
              "Most of a liquidation's filled quote value the protocol will spend",
              "reimbursing whoever cranked it, in basis points.",
              "",
              "A crank that nobody can afford to land is a liquidation that does not",
              "happen, and a fee market moves faster than any figure the protocol can",
              "keep written down. So the liquidation crank repays what the",
              "transaction actually cost — its base fee plus the priority fee it",
              "paid — and this bounds that at a share of what the liquidation",
              "recovered. Small liquidations stop being worth landing in heavy",
              "congestion, which is the right answer: the recovery does not cover the",
              "gas.",
              "",
              "Reimbursing a cost the keeper chooses is safe here because it is not a",
              "cost the keeper keeps: a priority fee goes to the validator, so",
              "bidding it up buys nothing. A keeper that is also the validator can",
              "recapture some of it, and this cap is what bounds that to a share the",
              "protocol chose.",
              "",
              "Zero disables reimbursement, leaving the flat payment."
            ],
            "type": "u16"
          },
          {
            "name": "solSpotMarketIndex",
            "docs": [
              "Spot market whose oracle prices SOL, for the one place the protocol",
              "pays lamports against a quote-denominated figure. Zero disables the",
              "reimbursement as surely as a zero share does: market zero is the quote",
              "market, which prices nothing useful here."
            ],
            "type": "u16"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                184
              ]
            }
          }
        ]
      }
    },
    {
      "name": "swapRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "user",
            "type": "pubkey"
          },
          {
            "name": "amountOut",
            "docs": [
              "precision: out market mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "amountIn",
            "docs": [
              "precision: in market mint precision"
            ],
            "type": "u64"
          },
          {
            "name": "outMarketIndex",
            "type": "u16"
          },
          {
            "name": "inMarketIndex",
            "type": "u16"
          },
          {
            "name": "outOraclePrice",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "inOraclePrice",
            "docs": [
              "precision: PRICE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "fee",
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "swapReduceOnly",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "in"
          },
          {
            "name": "out"
          }
        ]
      }
    },
    {
      "name": "syncLiqConditionsArgs",
      "docs": [
        "What the caller asks for; the terms the account ends up holding are",
        "[`SyncLiqConditionsTerms`], derived from these."
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "syncCostUnits",
            "docs": [
              "Cost units the staged self-sync requests, measured by simulating it.",
              "Priced against `State.transaction_fee_rails`. 0 keeps the watch/poll",
              "conditions inactive (manual syncs only) — turners have no signal to",
              "take unpaid work."
            ],
            "type": "u32"
          },
          {
            "name": "syncFallbackSlots",
            "docs": [
              "Coarse fallback interval, in slots. 0 = use the previous value."
            ],
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "takerOriginCrossRecordV0",
      "docs": [
        "Emitted when `crank_taker_origin_cross` resolves a taker-origin cross on a",
        "CLOB book: what the taker gained by settling at the counterparty's price",
        "instead of its own, and what the cranker took out of that.",
        "",
        "The fill itself also emits the ordinary `OrderActionRecord` for the match.",
        "This record carries what that one structurally cannot: the price the order",
        "was *resting* at (an `OrderActionRecord` only ever knows the price it",
        "filled at), the improvement between the two, and the crank reward — which",
        "is charged to the taker out of the improvement rather than carved out of",
        "the taker fee, so it never appears as that record's `filler_reward`."
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "docs": [
              "unix_timestamp of action"
            ],
            "type": "i64"
          },
          {
            "name": "slot",
            "type": "u64"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "taker",
            "docs": [
              "owner of the taker-origin order that aggressed this match — the later of",
              "the two to rest when both sides were taker-origin"
            ],
            "type": "pubkey"
          },
          {
            "name": "maker",
            "docs": [
              "the counterparty, filled at its own price"
            ],
            "type": "pubkey"
          },
          {
            "name": "filler",
            "docs": [
              "the cranker's `User`, credited `crank_reward` in quote"
            ],
            "type": "pubkey"
          },
          {
            "name": "baseAssetAmount",
            "type": "u64"
          },
          {
            "name": "quoteAssetAmount",
            "type": "u64"
          },
          {
            "name": "restPrice",
            "docs": [
              "the price the taker-origin order was resting at"
            ],
            "type": "u64"
          },
          {
            "name": "fillPrice",
            "docs": [
              "the counterparty's price — what the match settled at"
            ],
            "type": "u64"
          },
          {
            "name": "improvement",
            "docs": [
              "gross quote the taker gained: |rest_price − fill_price| × base"
            ],
            "type": "u64"
          },
          {
            "name": "crankReward",
            "docs": [
              "quote paid to the cranker out of that improvement"
            ],
            "type": "u64"
          },
          {
            "name": "makerTakerOrigin",
            "docs": [
              "the counterparty was itself a migrated taker remainder, and won the",
              "price by resting first — so this match was two remainders clearing",
              "against each other rather than one against an ordinary maker"
            ],
            "type": "bool"
          },
          {
            "name": "remainderBaseAssetAmount",
            "docs": [
              "size the match was too small to consume, put back on the book still",
              "taker-origin (0 when the cross consumed both orders outright, or when",
              "the leftover was below the book's minimum and was dropped)"
            ],
            "type": "u64"
          },
          {
            "name": "remainderOrderId",
            "docs": [
              "the re-placed remainder's new CLOB order id (0 when nothing was",
              "re-placed) — the old handle is stale, this is the client's new one"
            ],
            "type": "u64"
          },
          {
            "name": "remainderOwner",
            "docs": [
              "whose remainder was re-placed: `taker` or `maker` above (the default",
              "pubkey when nothing was). Only a match between two remainders can leave",
              "it on the maker, since an ordinary counterparty is consumed to exactly",
              "the size the cross was priced for"
            ],
            "type": "pubkey"
          }
        ]
      }
    },
    {
      "name": "targetsDatum",
      "serialization": "bytemuck",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "costToTradeBps",
            "type": "i32"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                4
              ]
            }
          },
          {
            "name": "targetBase",
            "type": "i64"
          },
          {
            "name": "lastOracleSlot",
            "type": "u64"
          },
          {
            "name": "lastPositionSlot",
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "transactionFeeRails",
      "docs": [
        "What the network charges to land one transaction, split the way the fee",
        "model splits it.",
        "",
        "Relay cranks pay their keeper out of a reservoir, and the payment has to",
        "cover the keeper's own transaction or nobody cranks. The cost is a function",
        "of what the transaction asks for: a fixed charge to be included, plus a rate",
        "on the cost units it requests. A crank's cost units differ by an order of",
        "magnitude between a book removal and a two-legged cross, and the rate is",
        "the network's to change, so every payment is derived from these fields",
        "rather than set beside them.",
        "",
        "Setting `resource_fee_denominator` to zero prices resource units at nothing,",
        "which is the fee model that charges per signature alone."
      ],
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "inclusionLamports",
            "docs": [
              "Charged once per transaction, whatever it contains."
            ],
            "type": "u32"
          },
          {
            "name": "signatureLamports",
            "docs": [
              "Charged per signature the transaction carries."
            ],
            "type": "u32"
          },
          {
            "name": "resourceFeeNumerator",
            "docs": [
              "Lamports per requested cost unit, as a fraction. Rounded up: a payment",
              "short by a lamport is a crank nobody runs."
            ],
            "type": "u32"
          },
          {
            "name": "resourceFeeDenominator",
            "docs": [
              "Zero prices cost units at nothing."
            ],
            "type": "u32"
          }
        ]
      }
    },
    {
      "name": "transferFeeAndPnlPoolDirection",
      "type": {
        "kind": "enum",
        "variants": [
          {
            "name": "feeToPnlPool"
          },
          {
            "name": "pnlToFeePool"
          }
        ]
      }
    },
    {
      "name": "transferFeeAndPnlPoolRecord",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "ts",
            "type": "i64"
          },
          {
            "name": "slot",
            "type": "u64"
          },
          {
            "name": "perpMarketIndexWithFeePool",
            "type": "u16"
          },
          {
            "name": "perpMarketIndexWithPnlPool",
            "type": "u16"
          },
          {
            "name": "direction",
            "type": {
              "defined": {
                "name": "transferFeeAndPnlPoolDirection"
              }
            }
          },
          {
            "name": "amount",
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "triggerSlotMetaV0",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "quoter",
            "docs": [
              "The market's canonical CLOB entry / book / program — set when this",
              "slot's executor is `trigger_clob_order`, zeroed for `trigger_order`."
            ],
            "type": "pubkey"
          },
          {
            "name": "clobMarket",
            "type": "pubkey"
          },
          {
            "name": "clobProgram",
            "type": "pubkey"
          },
          {
            "name": "orderId",
            "type": "u32"
          },
          {
            "name": "marketIndex",
            "type": "u16"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                2
              ]
            }
          }
        ]
      }
    },
    {
      "name": "updatePerpMarketSummaryStatsParams",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "netUnsettledFundingPnl",
            "type": {
              "option": "i64"
            }
          },
          {
            "name": "updateAmmSummaryStats",
            "type": {
              "option": "bool"
            }
          }
        ]
      }
    },
    {
      "name": "updateQuoterAccountsArgs",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "leg",
            "type": {
              "defined": {
                "name": "quoterCpiLeg"
              }
            }
          },
          {
            "name": "index",
            "docs": [
              "Slot in the registered list this slice starts at."
            ],
            "type": "u8"
          },
          {
            "name": "metas",
            "type": {
              "vec": {
                "defined": {
                  "name": "quoterAccountMetaArg"
                }
              }
            }
          }
        ]
      }
    },
    {
      "name": "updateQuoterConfigArgs",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "responseAccount",
            "type": {
              "option": "pubkey"
            }
          },
          {
            "name": "quoteV0Discriminator",
            "type": {
              "option": {
                "array": [
                  "u8",
                  8
                ]
              }
            }
          },
          {
            "name": "quoteL3V0Discriminator",
            "docs": [
              "Set to all-zero to withdraw the leg."
            ],
            "type": {
              "option": {
                "array": [
                  "u8",
                  8
                ]
              }
            }
          },
          {
            "name": "executeV0Discriminator",
            "type": {
              "option": {
                "array": [
                  "u8",
                  8
                ]
              }
            }
          }
        ]
      }
    },
    {
      "name": "updateQuoterWatchArgs",
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "watchOffset",
            "type": "u32"
          },
          {
            "name": "watchLen",
            "docs": [
              "0 clears the declaration (poll-only discovery)."
            ],
            "type": "u32"
          }
        ]
      }
    },
    {
      "name": "user",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "authority",
            "docs": [
              "The owner/authority of the account"
            ],
            "type": "pubkey"
          },
          {
            "name": "delegate",
            "docs": [
              "An addresses that can control the account on the authority's behalf. Has limited power, cant withdraw"
            ],
            "type": "pubkey"
          },
          {
            "name": "name",
            "docs": [
              "Encoded display name e.g. \"toly\""
            ],
            "type": {
              "array": [
                "u8",
                32
              ]
            }
          },
          {
            "name": "spotPositions",
            "docs": [
              "The user's spot positions"
            ],
            "type": {
              "array": [
                {
                  "defined": {
                    "name": "spotPosition"
                  }
                },
                8
              ]
            }
          },
          {
            "name": "perpPositions",
            "docs": [
              "The user's perp positions"
            ],
            "type": {
              "array": [
                {
                  "defined": {
                    "name": "perpPosition"
                  }
                },
                8
              ]
            }
          },
          {
            "name": "orders",
            "docs": [
              "The user's orders"
            ],
            "type": {
              "array": [
                {
                  "defined": {
                    "name": "order"
                  }
                },
                32
              ]
            }
          },
          {
            "name": "totalDeposits",
            "docs": [
              "The total values of deposits the user has made",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "totalWithdraws",
            "docs": [
              "The total values of withdrawals the user has made",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "totalSocialLoss",
            "docs": [
              "The total socialized loss the users has incurred upon the protocol",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "settledPerpPnl",
            "docs": [
              "Fees (taker fees, maker rebate, referrer reward, filler reward) and pnl for perps",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "cumulativeSpotFees",
            "docs": [
              "Fees (taker fees, maker rebate, filler reward) for spot",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "cumulativePerpFunding",
            "docs": [
              "Cumulative funding paid/received for perps",
              "precision: QUOTE_PRECISION"
            ],
            "type": "i64"
          },
          {
            "name": "liquidationMarginFreed",
            "docs": [
              "The amount of margin freed during liquidation. Used to force the liquidation to occur over a period of time",
              "Defaults to zero when not being liquidated",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "lastActiveSlot",
            "docs": [
              "The last slot a user was active. Used to determine if a user is idle"
            ],
            "type": "u64"
          },
          {
            "name": "nextOrderId",
            "docs": [
              "Every user order has an order id. This is the next order id to be used"
            ],
            "type": "u32"
          },
          {
            "name": "maxMarginRatio",
            "docs": [
              "Custom max initial margin ratio for the user"
            ],
            "type": "u32"
          },
          {
            "name": "nextLiquidationId",
            "docs": [
              "The next liquidation id to be used for user"
            ],
            "type": "u16"
          },
          {
            "name": "subAccountId",
            "docs": [
              "The sub account id for this user"
            ],
            "type": "u16"
          },
          {
            "name": "status",
            "docs": [
              "Whether the user is active, being liquidated or bankrupt"
            ],
            "type": "u8"
          },
          {
            "name": "isMarginTradingEnabled",
            "docs": [
              "Whether the user has enabled margin trading"
            ],
            "type": "bool"
          },
          {
            "name": "idle",
            "docs": [
              "User is idle if they haven't interacted with the protocol in 1 week and they have no orders, perp positions or borrows",
              "Off-chain keeper bots can ignore users that are idle"
            ],
            "type": "bool"
          },
          {
            "name": "openOrders",
            "docs": [
              "number of open orders"
            ],
            "type": "u8"
          },
          {
            "name": "hasOpenOrder",
            "docs": [
              "Whether or not user has open order"
            ],
            "type": "bool"
          },
          {
            "name": "openAuctions",
            "docs": [
              "number of open orders with auction"
            ],
            "type": "u8"
          },
          {
            "name": "hasOpenAuction",
            "docs": [
              "Whether or not user has open order with auction"
            ],
            "type": "bool"
          },
          {
            "name": "poolId",
            "type": "u8"
          },
          {
            "name": "specialUserStatus",
            "docs": [
              "Whether the user is a special user (vamm hedger, etc)"
            ],
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                3
              ]
            }
          },
          {
            "name": "equityFloor",
            "docs": [
              "Minimum account net equity (unweighted assets plus perp pnl minus",
              "spot liabilities, see `calculate_user_equity`). Below this the",
              "permissionless breaker can trip. Risk-increasing orders, fills,",
              "withdrawals and deposit transfers must clear `equity_floor +",
              "equity_floor_buffer`. Settable only by the warm/cold admin; 0 disables",
              "both checks.",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "equityFloorBuffer",
            "docs": [
              "Extra headroom above `equity_floor` required by risk-increasing",
              "actions, so an account cannot legally end an action at the trip",
              "threshold. No effect while `equity_floor` is 0.",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "userConditionsV0",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "relay",
            "docs": [
              "Everything relay needs hosted, in one field: the `relay-spec` header,",
              "the condition slots, and the shared sync account list (see",
              "[`LIQ_SYNC_ACCOUNTS_MAX`]). First field, so its watch offset is 8."
            ],
            "type": {
              "defined": {
                "name": "relayBlock11x32",
                "generics": [
                  {
                    "kind": "const",
                    "value": "11"
                  },
                  {
                    "kind": "const",
                    "value": "32"
                  }
                ]
              }
            }
          },
          {
            "name": "triggerSlots",
            "docs": [
              "Parallel to the trigger condition slots."
            ],
            "type": {
              "array": [
                {
                  "defined": {
                    "name": "triggerSlotMetaV0"
                  }
                },
                8
              ]
            }
          },
          {
            "name": "triggerResolvers",
            "docs": [
              "Per-slot trigger resolver lists (see [`TRIGGER_RESOLVERS_LEN`])."
            ],
            "type": {
              "array": [
                "u8",
                1344
              ]
            }
          },
          {
            "name": "user",
            "docs": [
              "The `User` these conditions watch."
            ],
            "type": "pubkey"
          },
          {
            "name": "syncPaymentLamports",
            "docs": [
              "Fee the sync executor pays its keeper out of the protocol crank",
              "treasury.",
              "",
              "Stated by whoever opts in, and capped at",
              "[`LIQ_SYNC_MAX_COST_UNITS`] when it is priced, because opting in is",
              "permissionless and the payer is protocol funds rather than the account",
              "itself. [`Self::last_paid_sync_slot`] bounds how often it can be drawn."
            ],
            "type": "u64"
          },
          {
            "name": "syncFallbackSlots",
            "docs": [
              "The fallback poll interval."
            ],
            "type": "u64"
          },
          {
            "name": "positionsDigest",
            "docs": [
              "Digest of the exposures the last sync ran against. The resolver",
              "compares it to the user's current positions to decide staleness —",
              "comparing *watched markets* instead never converges for a user",
              "whose exposures produce no watchable threshold (an unsupported",
              "oracle layout, a market with no reservoir), leaving the",
              "level-triggered sync wake firing forever. The localnet harness",
              "caught exactly that loop, once a second."
            ],
            "type": "u64"
          },
          {
            "name": "lastPaidSyncSlot",
            "docs": [
              "Slot the treasury last paid a keeper for resyncing this account.",
              "",
              "A resync is paid at most once per [`Self::sync_fallback_slots`], which",
              "is the cadence the fallback poll already runs at. Opting in is",
              "permissionless and the treasury pays, so without this anyone could",
              "crank the same account in a loop and draw the fee every time — real",
              "work is not required for the instruction to succeed, only for it to be",
              "worth paying for."
            ],
            "type": "u64"
          },
          {
            "name": "padding",
            "docs": [
              "Tail reserve: 8 bytes of alignment slack plus room for two more",
              "pubkeys, so a future sync input can be captured here instead of",
              "forcing an `extend_account` migration on every opted-in user."
            ],
            "type": {
              "array": [
                "u8",
                64
              ]
            }
          }
        ]
      }
    },
    {
      "name": "userFees",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "totalFeePaid",
            "docs": [
              "Total taker fee paid",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "totalFeeRebate",
            "docs": [
              "Total maker fee rebate",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "totalTokenDiscount",
            "docs": [
              "Total discount from holding token",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "totalRefereeDiscount",
            "docs": [
              "Total discount from being referred",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          }
        ]
      }
    },
    {
      "name": "userStats",
      "serialization": "bytemuckunsafe",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "authority",
            "docs": [
              "The authority for all of a users sub accounts"
            ],
            "type": "pubkey"
          },
          {
            "name": "referrer",
            "docs": [
              "The address that referred this user"
            ],
            "type": "pubkey"
          },
          {
            "name": "fees",
            "docs": [
              "Stats on the fees paid by the user"
            ],
            "type": {
              "defined": {
                "name": "userFees"
              }
            }
          },
          {
            "name": "makerVolume30d",
            "docs": [
              "Rolling 30day maker volume for user",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "takerVolume30d",
            "docs": [
              "Rolling 30day taker volume for user",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "fillerVolume30d",
            "docs": [
              "Rolling 30day filler volume for user",
              "precision: QUOTE_PRECISION"
            ],
            "type": "u64"
          },
          {
            "name": "lastMakerVolume30dTs",
            "docs": [
              "last time the maker volume was updated"
            ],
            "type": "i64"
          },
          {
            "name": "lastTakerVolume30dTs",
            "docs": [
              "last time the taker volume was updated"
            ],
            "type": "i64"
          },
          {
            "name": "lastFillerVolume30dTs",
            "docs": [
              "last time the filler volume was updated"
            ],
            "type": "i64"
          },
          {
            "name": "ifStakedQuoteAssetAmount",
            "docs": [
              "The amount of tokens staked in the quote spot markets if"
            ],
            "type": "u64"
          },
          {
            "name": "numberOfSubAccounts",
            "docs": [
              "The current number of sub accounts"
            ],
            "type": "u16"
          },
          {
            "name": "numberOfSubAccountsCreated",
            "docs": [
              "The number of sub accounts created. Can be greater than the number of sub accounts if user",
              "has deleted sub accounts"
            ],
            "type": "u16"
          },
          {
            "name": "referrerStatus",
            "docs": [
              "Flags for referrer status:",
              "First bit (LSB): 1 if user is a referrer, 0 otherwise",
              "Second bit: 1 if user was referred, 0 otherwise"
            ],
            "type": "u8"
          },
          {
            "name": "disableUpdatePerpBidAskTwap",
            "type": "u8"
          },
          {
            "name": "pausedOperations",
            "type": "u8"
          },
          {
            "name": "padding1",
            "docs": [
              "9 bytes: 1 byte of former repr(C) alignment padding + the removed",
              "8-byte `if_staked_gov_token_amount` field (gov-token stake fee discount)"
            ],
            "type": {
              "array": [
                "u8",
                9
              ]
            }
          },
          {
            "name": "delegatePermissions",
            "docs": [
              "Delegate permissions across all sub accounts"
            ],
            "type": "u8"
          },
          {
            "name": "equityBreakerTripped",
            "docs": [
              "Set by the permissionless `trip_equity_floor_breaker` instruction when",
              "any of the authority's subaccounts falls below its equity floor.",
              "While set, every subaccount of the authority rejects risk-increasing",
              "fills, withdrawals and transfers out. Cleared only by the warm admin."
            ],
            "type": "u8"
          },
          {
            "name": "padding",
            "type": {
              "array": [
                "u8",
                62
              ]
            }
          }
        ]
      }
    },
    {
      "name": "validityGuardRails",
      "repr": {
        "kind": "c"
      },
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "slotsBeforeStaleForAmm",
            "type": "i64"
          },
          {
            "name": "slotsBeforeStaleForMargin",
            "type": "i64"
          },
          {
            "name": "confidenceIntervalMaxSize",
            "type": "u64"
          },
          {
            "name": "tooVolatileRatio",
            "type": "i64"
          }
        ]
      }
    },
    {
      "name": "signedMsgOrderParamsExport",
      "docs": [
        "unusued placeholder event to force include signed msg types into velocity IDL"
      ],
      "type": {
        "kind": "struct",
        "fields": [
          {
            "name": "a",
            "type": {
              "defined": {
                "name": "signedMsgOrderParamsMessage"
              }
            }
          },
          {
            "name": "b",
            "type": {
              "defined": {
                "name": "signedMsgOrderParamsDelegateMessage"
              }
            }
          }
        ]
      }
    }
  ]
};
