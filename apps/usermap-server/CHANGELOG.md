# @velocity-exchange/usermap-server

## 0.1.7

### Patch Changes

- [#214](https://github.com/velocity-exchange/velocity-v1/pull/214) [`f7b2eb6`](https://github.com/velocity-exchange/velocity-v1/commit/f7b2eb6a133e04816505f24d8ab5ea6fecf458b5) Thanks [@jordy25519](https://github.com/jordy25519)! - Fix usermap publisher crash under gRPC. Call `client.connect()` before `subscribe()` (yellowstone-grpc 5.x requires an explicit dial, otherwise it throws "Client not connected. Call connect() first"). Also stop retrying the whole `main()` on failure — only the subscription is retried now, so a transient failure no longer re-runs `server.listen(:5001)` and crashes with `EADDRINUSE` (an unhandled `'error'` event that bypassed the retry). Fatal setup errors now exit the process for a clean pod restart.

- [#215](https://github.com/velocity-exchange/velocity-v1/pull/215) [`7947f6c`](https://github.com/velocity-exchange/velocity-v1/commit/7947f6cb077a67b8b2f627134e49cf71e33c703a) Thanks [@jordy25519](https://github.com/jordy25519)! - Fix usermap publisher gRPC health/liveness on low-activity markets. The subscribe request used `slots: {}` (an empty map = no slot subscription), so the health check — which only learns the current slot from incoming account writes — reported "slot lag" and resubscribed every 30s whenever user accounts were idle. Subscribe to the slot stream and advance the liveness markers from slot updates, so health tracks chain progress rather than account activity.

- Updated dependencies [[`00decfd`](https://github.com/velocity-exchange/velocity-v1/commit/00decfd93fff5668779255288a0f61242be99d07), [`7b44bb0`](https://github.com/velocity-exchange/velocity-v1/commit/7b44bb0832e3eb72b2cd31d01d3c7d60c9794dab), [`c288314`](https://github.com/velocity-exchange/velocity-v1/commit/c2883143d813a5c608354d64c3d1a5b825c4f398), [`61cbeb2`](https://github.com/velocity-exchange/velocity-v1/commit/61cbeb2f70dcd3f6d4fab1209962132dea9d60fe), [`7b44bb0`](https://github.com/velocity-exchange/velocity-v1/commit/7b44bb0832e3eb72b2cd31d01d3c7d60c9794dab), [`155ed06`](https://github.com/velocity-exchange/velocity-v1/commit/155ed0618d7012b9305f4c84786c4d4e96d86be8)]:
  - @velocity-exchange/sdk@0.6.0

## 0.1.6

### Patch Changes

- Updated dependencies [[`9854dfa`](https://github.com/velocity-exchange/velocity-v1/commit/9854dfa1c915938fe08495568f262285ffb6c933), [`6d58632`](https://github.com/velocity-exchange/velocity-v1/commit/6d58632540814c739fc3848e4110c2a24547722b), [`3042967`](https://github.com/velocity-exchange/velocity-v1/commit/304296799e8d5b8a6525cb18cfd8398637c00521)]:
  - @velocity-exchange/sdk@0.5.0

## 0.1.5

### Patch Changes

- Updated dependencies [[`dff8a47`](https://github.com/velocity-exchange/velocity-v1/commit/dff8a4754f6b736fed330b2ab4fa5685db4f8159), [`900c07d`](https://github.com/velocity-exchange/velocity-v1/commit/900c07d9da7e106c82fbe65b3d92226d090bdee9), [`8df28ac`](https://github.com/velocity-exchange/velocity-v1/commit/8df28ac6d113760ae4a8cdff4ad438cb25efce2c), [`bafd699`](https://github.com/velocity-exchange/velocity-v1/commit/bafd6990f8322f232d2f0d17042beb0e9c567164)]:
  - @velocity-exchange/sdk@0.4.0

## 0.1.4

### Patch Changes

- Updated dependencies [[`b7d15b9`](https://github.com/velocity-exchange/velocity-v1/commit/b7d15b970a74d267aeaf20bb644d5344b9aadc61), [`2f6c64d`](https://github.com/velocity-exchange/velocity-v1/commit/2f6c64d54f1146d8e7f9ee4ab556929c6bf8b920), [`3f148f8`](https://github.com/velocity-exchange/velocity-v1/commit/3f148f8b477e4176e11e0660adb0e67dd5163d3b)]:
  - @velocity-exchange/sdk@0.3.0

## 0.1.3

### Patch Changes

- Updated dependencies [[`d3b58ab`](https://github.com/velocity-exchange/velocity-v1/commit/d3b58ab7e3ad33f0e6634ff87e9b150915b3aa13)]:
  - @velocity-exchange/sdk@0.2.6

## 0.1.2

### Patch Changes

- Updated dependencies [[`15073bc`](https://github.com/velocity-exchange/velocity-v1/commit/15073bc0b740b2d1cad471126a00368e72655bd5)]:
  - @velocity-exchange/sdk@0.2.5

## 0.1.1

### Patch Changes

- Updated dependencies [[`4f8e7aa`](https://github.com/velocity-exchange/velocity-v1/commit/4f8e7aaef0e35b190fc0b91cd29314d902d1ccab)]:
  - @velocity-exchange/sdk@0.2.4
