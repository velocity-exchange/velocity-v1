/* eslint-disable @typescript-eslint/no-non-null-assertion */
import {
	DLOB,
	SlotSubscriber,
	MarketType,
	VelocityClient,
	loadKeypair,
	calculateBidPrice,
	calculateAskPrice,
	UserAccount,
	isVariant,
	decodeUser,
	Wallet,
	BN,
	ClockSubscriber,
	SignedMsgOrderParamsMessage,
	getUserAccountPublicKey,
	SignedMsgOrderNode,
	Order,
	ZERO,
	OrderTriggerCondition,
	PositionDirection,
	OraclePriceData,
	OrderStatus,
	getVariant,
	PRICE_PRECISION,
	convertToNumber,
	BASE_PRECISION,
	SignedMsgOrderParamsDelegateMessage,
	OrderParamsBitFlag,
	PerpMarketAccount,
	SpotMarketAccount,
	elapsedMillis,
	currentSlotDuration,
	signedMsgOrderMaxSlot,
	signedMsgOrderPlaceable,
} from '@velocity-exchange/sdk';
import { Connection, PublicKey } from '@solana/web3.js';
import dotenv from 'dotenv';
import parseArgs from 'minimist';
import { logger } from '../../logger';
import {
	FallbackLiquiditySource,
	SerializedNodeToFill,
	NodeToFillWithContext,
} from './types';
import { getVelocityClientFromArgs, serializeNodeToFill } from './utils';
import { sleepMs } from '../../utils';
import { LRUCache } from 'lru-cache';
import { sha256 } from '@noble/hashes/sha256';

const EXPIRE_ORDER_BUFFER_SEC = 30; // add an extra 30 seconds before trying to expire orders (want to avoid 6252 error due to clock velocity)

const logPrefix = '[DLOBBuilder]';
class DLOBBuilder {
	private userAccountData = new Map<string, UserAccount>();
	private userAccountDataBuffers = new Map<string, Buffer>();
	private dlob: DLOB;
	public readonly slotSubscriber: SlotSubscriber;
	public readonly marketTypeString: string;
	public readonly marketType: MarketType;
	public readonly marketIndexes: number[];
	public velocityClient: VelocityClient;
	public initialized: boolean = false;

	private clockSubscriber: ClockSubscriber;

	private signedMsgUserAuthorities = new Map<string, string>();

	// SignedMsg orders to keep track of
	private signedMsgOrders = new LRUCache<number, SignedMsgOrderNode>({
		max: 5000,
		dispose: (_, key) => {
			if (typeof process.send === 'function') {
				process.send({
					type: 'signedMsgOrderParamsMessage',
					data: {
						uuid: key,
						type: 'delete',
					},
				});
			}
			return;
		},
	});

	constructor(
		velocityClient: VelocityClient,
		marketType: MarketType,
		marketTypeString: string,
		marketIndexes: number[]
	) {
		this.dlob = new DLOB();
		// Same reason as the ClockSubscriber below: a frozen websocket would freeze
		// this slot, which now decides when a signed-msg order enters the book.
		this.slotSubscriber = new SlotSubscriber(velocityClient.connection, {
			resubTimeoutMs: 10_000,
		});
		this.marketType = marketType;
		this.marketTypeString = marketTypeString;
		this.marketIndexes = marketIndexes;
		this.velocityClient = velocityClient;

		this.clockSubscriber = new ClockSubscriber(velocityClient.connection, {
			commitment: 'confirmed',
			resubTimeoutMs: 5_000,
		});
	}

	public async subscribe() {
		await this.slotSubscriber.subscribe();
		await this.clockSubscriber.subscribe();
	}

	public getUserBuffer(pubkey: string) {
		return this.userAccountDataBuffers.get(pubkey);
	}

	public deserializeAndUpdateUserAccountData(
		userAccount: string,
		pubkey: string
	) {
		const userAccountBuffer = Buffer.from(userAccount, 'base64');
		const deserializedUserAccount = decodeUser(userAccountBuffer);
		this.userAccountDataBuffers.set(pubkey, userAccountBuffer);
		this.userAccountData.set(pubkey, deserializedUserAccount);
	}

	public delete(pubkey: string) {
		this.userAccountData.delete(pubkey);
		this.userAccountDataBuffers.delete(pubkey);
	}

	// Private to avoid race conditions
	private build(): DLOB {
		logger.debug(
			`${logPrefix} Building DLOB with ${this.userAccountData.size} users`
		);
		const dlob = new DLOB();
		try {
			// auction wall clock math converts elapsed slots through the State
			// slot clock; unsubscribed state falls back to the 400ms baseline
			dlob.slotDurationState = this.velocityClient.getStateAccount();
		} catch {
			// not subscribed yet: keep the baseline
		}
		const slot = this.slotSubscriber.getSlot();
		let counter = 0;
		this.userAccountData.forEach((userAccount, pubkey) => {
			userAccount.orders.forEach((order) => {
				if (
					!this.marketIndexes.includes(order.marketIndex) ||
					!isVariant(order.marketType, this.marketTypeString.toLowerCase())
				) {
					return;
				}
				dlob.insertOrder(order, pubkey, slot, order.baseAssetAmount);
				counter++;
			});
		});
		for (const signedMsgNode of this.signedMsgOrders.values()) {
			// Hold back an auction order whose signed message slot has not arrived: the
			// program starts its auction there and rejects a place before it, so it
			// cannot fill yet. Inserting it early lets the taking pass match it against
			// resting liquidity and mark that liquidity filled in this snapshot, hiding
			// a fill that could have happened. It stays cached and is inserted once its
			// slot lands. A resting limit (no auction) may be placed ahead of its slot,
			// though this builder never caches one (insertSignedMsgOrder skips it).
			if (
				!signedMsgOrderPlaceable(
					dlob.slotDurationState,
					signedMsgNode.order,
					slot
				)
			) {
				continue;
			}
			dlob.insertSignedMsgOrder(signedMsgNode.order, signedMsgNode.userAccount);
			counter++;
		}
		logger.debug(`${logPrefix} Built DLOB with ${counter} orders`);
		this.dlob = dlob;
		return dlob;
	}

	public async insertSignedMsgOrder(orderData: any, uuid: number) {
		// Deserialize and store
		const signedMsgOrderParamsBuf = Buffer.from(
			orderData['order_message'],
			'hex'
		);
		const isDelegateSigner = signedMsgOrderParamsBuf
			.slice(0, 8)
			.equals(
				Uint8Array.from(
					Buffer.from(
						sha256('global' + ':' + 'SignedMsgOrderParamsDelegateMessage')
					).slice(0, 8)
				)
			);
		const signedMessage:
			| SignedMsgOrderParamsMessage
			| SignedMsgOrderParamsDelegateMessage =
			this.velocityClient.decodeSignedMsgOrderParamsMessage(
				signedMsgOrderParamsBuf,
				isDelegateSigner
			);

		const signedMsgOrderParams = signedMessage.signedMsgOrderParams;

		if (
			!signedMsgOrderParams.auctionDuration ||
			!signedMsgOrderParams.auctionStartPrice ||
			!signedMsgOrderParams.auctionEndPrice
		) {
			return;
		}

		if (signedMsgOrderParams.baseAssetAmount.eq(ZERO)) {
			return;
		}

		const takerAuthority = new PublicKey(orderData['taker_authority']);
		const takerUserPubkey = isDelegateSigner
			? (signedMessage as SignedMsgOrderParamsDelegateMessage).takerPubkey
			: await getUserAccountPublicKey(
					this.velocityClient.program.programId,
					takerAuthority,
					(signedMessage as SignedMsgOrderParamsMessage).subAccountId
			  );
		logger.info(
			`Received signedMsgOrder: pubkey: ${takerUserPubkey.toString()}, direction: ${getVariant(
				signedMsgOrderParams.direction
			)}, marketIndex: ${
				signedMsgOrderParams.marketIndex
			}, baseAssetAmount: ${convertToNumber(
				signedMsgOrderParams.baseAssetAmount,
				BASE_PRECISION
			)}, auctionDuration: ${
				signedMsgOrderParams.auctionDuration
			}, auctionStartPrice: ${convertToNumber(
				signedMsgOrderParams.auctionStartPrice,
				PRICE_PRECISION
			)}, auctionEndPrice: ${convertToNumber(
				signedMsgOrderParams.auctionEndPrice,
				PRICE_PRECISION
			)}, maxTs: ${signedMsgOrderParams.maxTs}`
		);

		this.signedMsgUserAuthorities.set(
			takerUserPubkey.toString(),
			orderData['signing_authority']
		);

		const slotDurationState = this.velocityClient.getStateAccount();
		const maxSlot = signedMsgOrderMaxSlot(
			slotDurationState,
			signedMessage.slot,
			signedMsgOrderParams.auctionDuration ?? 0
		);
		if (maxSlot.toNumber() < this.slotSubscriber.getSlot()) {
			logger.warn(
				`${logPrefix} Received expired signedMsg order with uuid: ${uuid}`
			);
			return;
		}

		const signedMsgOrder: Order = {
			status: OrderStatus.OPEN,
			orderType: signedMsgOrderParams.orderType,
			orderId: uuid,
			// The true message slot, which the UI stamps a few slots ahead of signing
			// (a signing buffer). It must not be clamped to the current slot: the
			// program starts the auction at it and rejects a place while
			// `order_slot > clock.slot`, so a clamped slot made a not-yet-valid order
			// look immediately fillable and burned the filler's single place+fill
			// attempt. Auction math reads a future slot as 0% progress, and the DLOB
			// derives its own max-slot eviction from it, so both need the real value.
			slot: signedMessage.slot,
			marketIndex: signedMsgOrderParams.marketIndex,
			marketType: MarketType.PERP,
			baseAssetAmount: signedMsgOrderParams.baseAssetAmount,
			auctionDuration: signedMsgOrderParams.auctionDuration,
			auctionStartPrice: signedMsgOrderParams.auctionStartPrice,
			auctionEndPrice: signedMsgOrderParams.auctionEndPrice,
			immediateOrCancel:
				(signedMsgOrderParams.bitFlags &
					OrderParamsBitFlag.ImmediateOrCancel) !==
				0,
			direction: signedMsgOrderParams.direction,
			postOnly: false,
			oraclePriceOffset: signedMsgOrderParams.oraclePriceOffset ?? ZERO,
			maxTs: signedMsgOrderParams.maxTs ?? ZERO,
			reduceOnly: signedMsgOrderParams.reduceOnly ?? false,
			triggerCondition:
				signedMsgOrderParams.triggerCondition ?? OrderTriggerCondition.ABOVE,
			price: signedMsgOrderParams.price ?? ZERO,
			userOrderId: signedMsgOrderParams.userOrderId ?? 0,
			// Rest are not necessary and set for type conforming
			existingPositionDirection: PositionDirection.LONG,
			triggerPrice: ZERO,
			baseAssetAmountFilled: ZERO,
			quoteAssetAmountFilled: ZERO,
			bitFlags: 0,
			postedSlotTail: 0,
		};

		const signedMsgOrderNode = new SignedMsgOrderNode(
			signedMsgOrder,
			takerUserPubkey.toString()
		);

		// Cache TTL uses the same piecewise interval, with the historical 25% pad.
		// Floored at one slot: the admission check above accepts an order whose max
		// slot is the current slot, whose remaining interval is 0, and lru-cache reads
		// a ttl of 0 as "never expires" - so a dying order would be emitted until
		// capacity eviction.
		const ttl = Math.max(
			Math.ceil(
				elapsedMillis(
					slotDurationState,
					new BN(this.slotSubscriber.getSlot()),
					maxSlot
				).toNumber() * 1.25
			),
			currentSlotDuration(this.velocityClient, this.slotSubscriber.getSlot())
		);
		this.signedMsgOrders.set(uuid, signedMsgOrderNode, {
			ttl,
		});
	}

	public getdNodesToFill(): NodeToFillWithContext[] {
		const dlob = this.build();
		const nodesToFill: NodeToFillWithContext[] = [];
		for (const marketIndex of this.marketIndexes) {
			let market: PerpMarketAccount | SpotMarketAccount | undefined;
			let oraclePriceData: OraclePriceData;
			let fallbackAsk: BN | undefined = undefined;
			let fallbackBid: BN | undefined = undefined;
			const fallbackAskSource: FallbackLiquiditySource | undefined = undefined;
			const fallbackBidSource: FallbackLiquiditySource | undefined = undefined;
			if (this.marketTypeString.toLowerCase() === 'perp') {
				market = this.velocityClient.getPerpMarketAccount(marketIndex);
				if (!market) {
					throw new Error('PerpMarket not found');
				}
				const mmOraclePriceData =
					this.velocityClient.getMMOracleDataForPerpMarket(marketIndex);
				oraclePriceData =
					this.velocityClient.getOracleDataForPerpMarket(marketIndex);
				fallbackBid = calculateBidPrice(
					market,
					mmOraclePriceData,
					new BN(this.slotSubscriber.getSlot()),
					this.velocityClient.getStateAccount()
				);
				fallbackAsk = calculateAskPrice(
					market,
					mmOraclePriceData,
					new BN(this.slotSubscriber.getSlot()),
					this.velocityClient.getStateAccount()
				);
			} else {
				market = this.velocityClient.getSpotMarketAccount(marketIndex);
				if (!market) {
					throw new Error('SpotMarket not found');
				}
				oraclePriceData =
					this.velocityClient.getOracleDataForSpotMarket(marketIndex);
			}

			const stateAccount = this.velocityClient.getStateAccount();
			if (!stateAccount) {
				throw new Error('State account not found');
			}
			const slot = this.slotSubscriber.getSlot();
			const unixTs = this.clockSubscriber.getUnixTs() ?? Date.now() / 1000;
			const nodesToFillForMarket = isVariant(this.marketType, 'perp')
				? dlob.findNodesToFill(
						marketIndex,
						fallbackBid,
						fallbackAsk,
						slot,
						unixTs - EXPIRE_ORDER_BUFFER_SEC,
						MarketType.PERP,
						this.velocityClient.getMMOracleDataForPerpMarket(marketIndex),
						stateAccount,
						market as PerpMarketAccount
				  )
				: dlob.findNodesToFill(
						marketIndex,
						fallbackBid,
						fallbackAsk,
						slot,
						unixTs - EXPIRE_ORDER_BUFFER_SEC,
						MarketType.SPOT,
						oraclePriceData,
						stateAccount,
						market as SpotMarketAccount
				  );

			nodesToFill.push(
				...nodesToFillForMarket.map((node) => {
					return { ...node, fallbackAskSource, fallbackBidSource };
				})
			);
		}
		return nodesToFill;
	}

	public serializeNodesToFill(
		nodesToFill: NodeToFillWithContext[]
	): SerializedNodeToFill[] {
		return nodesToFill
			.map((node) => {
				const buffer = this.getUserBuffer(node.node.userAccount!);
				if (!buffer && !node.node.isSignedMsg) {
					console.log(node.node);
					console.log(`Received node to fill without user account`);
					return undefined;
				}
				const makerBuffers = new Map<string, Buffer>();
				for (const makerNode of node.makerNodes) {
					const makerBuffer = this.getUserBuffer(makerNode.userAccount!);

					if (!makerBuffer) {
						return undefined;
					}
					makerBuffers.set(makerNode.userAccount!, makerBuffer);
				}
				return serializeNodeToFill(
					node,
					makerBuffers,
					// Protected-maker status removed from the SDK; always false.
					false,
					buffer,
					this.signedMsgUserAuthorities.get(node.node.userAccount!)
				);
			})
			.filter((node): node is SerializedNodeToFill => node !== undefined);
	}

	public trySendNodes(serializedNodesToFill: SerializedNodeToFill[]) {
		if (typeof process.send === 'function') {
			if (serializedNodesToFill.length > 0) {
				try {
					logger.debug('Sending fillable nodes');
					process.send({
						type: 'fillableNodes',
						data: serializedNodesToFill,
						// }, { swallowErrors: true });
					});
				} catch (e) {
					logger.error(`${logPrefix} Failed to send fillable nodes: ${e}`);
					// logger.error(JSON.stringify(serializedNodesToFill, null, 2));
				}
			}
		}
	}

	public removeConfirmedSignedMsgOrder(uuid: number) {
		this.signedMsgOrders.delete(uuid);
	}

	sendLivenessCheck(health: boolean) {
		if (typeof process.send === 'function') {
			process.send({
				type: 'health',
				data: {
					healthy: health,
				},
			});
		}
	}
}

const main = async () => {
	// kill this process if the parent dies
	process.on('disconnect', () => process.exit());

	dotenv.config();
	const endpoint = process.env.ENDPOINT;
	const privateKey = process.env.KEEPER_PRIVATE_KEY;
	const args = parseArgs(process.argv.slice(2));
	const velocityEnv = args['velocity-env'] ?? 'devnet';
	const marketTypeStr = args['market-type'];

	let marketIndexes;
	if (typeof args['market-indexes'] === 'string') {
		marketIndexes = args['market-indexes'].split(',').map(Number);
	} else {
		marketIndexes = [args['market-indexes']];
	}
	if (marketTypeStr !== 'perp' && marketTypeStr !== 'spot') {
		throw new Error("market-type must be either 'perp' or 'spot'");
	}

	let marketType: MarketType;
	switch (marketTypeStr) {
		case 'perp':
			marketType = MarketType.PERP;
			break;
		case 'spot':
			marketType = MarketType.SPOT;
			break;
		default:
			console.error('Error: Unsupported market type provided.');
			process.exit(1);
	}

	if (!endpoint || !privateKey) {
		throw new Error('ENDPOINT and KEEPER_PRIVATE_KEY must be provided');
	}
	const wallet = new Wallet(loadKeypair(privateKey));

	const connection = new Connection(endpoint, {
		wsEndpoint: process.env.WS_ENDPOINT,
		commitment: 'processed',
	});

	const velocityClient = getVelocityClientFromArgs({
		connection,
		wallet,
		marketIndexes,
		marketTypeStr,
		env: velocityEnv,
	});
	await velocityClient.subscribe();

	const dlobBuilder = new DLOBBuilder(
		velocityClient,
		marketType,
		marketTypeStr,
		marketIndexes
	);

	await dlobBuilder.subscribe();
	await sleepMs(5000); // Give the dlob some time to get built
	if (typeof process.send === 'function') {
		logger.info('DLOBBuilder started');
		process.send({ type: 'initialized', data: dlobBuilder.marketIndexes });
	}

	process.on('message', (msg: any) => {
		if (!msg.data || typeof msg.data.type === 'undefined') {
			logger.warn(`${logPrefix} Received message without data.type field.`);
			return;
		}
		switch (msg.data.type) {
			case 'signedMsgOrderParamsMessage':
				dlobBuilder.insertSignedMsgOrder(
					msg.data.signedMsgOrder,
					msg.data.uuid
				);
				break;
			case 'confirmed':
				dlobBuilder.removeConfirmedSignedMsgOrder(Number(msg.data.uuid));
				break;
			case 'update':
				dlobBuilder.deserializeAndUpdateUserAccountData(
					msg.data.userAccount,
					msg.data.pubkey
				);
				break;
			case 'delete':
				dlobBuilder.delete(msg.data.pubkey);
				break;
			default:
				logger.warn(
					`${logPrefix} Received unknown message type: ${msg.data.type}`
				);
		}
	});

	setInterval(() => {
		const nodesToFill = dlobBuilder.getdNodesToFill();
		const serializedNodesToFill = dlobBuilder.serializeNodesToFill(nodesToFill);
		logger.debug(
			`${logPrefix} Serialized ${serializedNodesToFill.length} fillable nodes`
		);
		dlobBuilder.trySendNodes(serializedNodesToFill);
	}, 200);

	dlobBuilder.sendLivenessCheck(true);
	setInterval(() => {
		dlobBuilder.sendLivenessCheck(true);
	}, 10_000);
};

main();
