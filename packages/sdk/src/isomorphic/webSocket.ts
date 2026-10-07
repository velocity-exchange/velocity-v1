/**
 * The `WebSocket` constructor every socket in the SDK builds from.
 *
 * `WebSocket` is a global in a browser and in node 22. The SDK supports node
 * 20, where it is not, so the `ws` package supplies it there. The `require`
 * runs only when the global is absent, which keeps a browser bundle working.
 */
// eslint-disable-next-line @typescript-eslint/no-var-requires
export const WebSocketImpl: typeof WebSocket =
	typeof globalThis !== 'undefined' && (globalThis as any).WebSocket
		? (globalThis as any).WebSocket
		: require('ws');
