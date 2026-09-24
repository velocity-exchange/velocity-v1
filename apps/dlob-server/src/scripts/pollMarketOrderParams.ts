/**
 bun run ts-node src/scripts/pollMarketOrderParams.ts [mainnet|staging]
 */

import { logger } from '../utils/logger';
import { setTimeout as sleep } from 'timers/promises';
import { request } from 'undici';
import * as fs from 'fs';
import * as path from 'path';

const envArg = process.argv[2];

const ENV = envArg ?? 'mainnet';
if (ENV !== 'mainnet' && ENV !== 'staging') {
	throw new Error(`Invalid environment: ${ENV}`);
}
// @ts-ignore
const TARGET_URL = `https://${
	ENV === 'staging' ? 'staging.' : ''
}dlob.velocity.exchange/marketOrderParams?assetType=base&marketIndex=2&direction=long&amount=550000000&reduceOnly=false&isOracleOrder=true`;

const OUTPUT_CSV = process.env.OUTPUT_CSV || `marketOrderParams-${ENV}.csv`;
const POLL_INTERVAL_MS = Number(process.env.POLL_INTERVAL_MS || 10_000);

const csvPath = path.resolve(process.cwd(), OUTPUT_CSV);

function ensureCsvHeader(): void {
	if (!fs.existsSync(csvPath)) {
		const header = [
			'timestamp_iso',
			'entryPrice',
			'bestPrice',
			'worstPrice',
			'oraclePrice',
			'markPrice',
			'priceImpact',
			'slippageTolerance',
			'orderType',
			'price',
			'oraclePriceOffset',
			'activationDelaySlots',
			'direction',
			'baseAssetAmount',
			'raw_json',
		].join(',');
		fs.writeFileSync(csvPath, header + '\n');
		logger.info(`Created CSV with header at ${csvPath}`);
	}
}

function csvEscape(value: unknown): string {
	if (value === null || value === undefined) return '';
	const str = String(value);
	if (/[",\n]/.test(str)) {
		return '"' + str.replace(/"/g, '""') + '"';
	}
	return str;
}

async function fetchMarketOrderParams(): Promise<any> {
	const { statusCode, body } = await request(TARGET_URL, { method: 'GET' });
	if (statusCode < 200 || statusCode >= 300) {
		throw new Error(`HTTP ${statusCode}`);
	}
	const text = await body.text();
	try {
		return JSON.parse(text);
	} catch (e) {
		throw new Error(`Invalid JSON: ${(e as Error).message}`);
	}
}

function appendRow(resp: any): void {
	const nowIso = new Date().toISOString();

	// Safely read expected fields if present
	const data = resp?.data ?? {};
	const entryPrice = data?.entryPrice ?? '';
	const bestPrice = data?.bestPrice ?? '';
	const worstPrice = data?.worstPrice ?? '';
	const oraclePrice = data?.oraclePrice ?? '';
	const markPrice = data?.markPrice ?? '';
	const priceImpact = data?.priceImpact ?? '';
	const slippageTolerance = data?.slippageTolerance ?? '';

	const params = data?.params ?? {};
	const orderType = params?.orderType ?? '';
	const price = params?.price ?? '';
	const oraclePriceOffset = params?.oraclePriceOffset ?? '';
	const activationDelaySlots = params?.activationDelaySlots ?? '';
	const direction = params?.direction ?? '';
	const baseAssetAmount = params?.baseAssetAmount ?? '';

	const raw = JSON.stringify(resp);

	const row = [
		csvEscape(nowIso),
		csvEscape(entryPrice),
		csvEscape(bestPrice),
		csvEscape(worstPrice),
		csvEscape(oraclePrice),
		csvEscape(markPrice),
		csvEscape(priceImpact),
		csvEscape(slippageTolerance),
		csvEscape(orderType),
		csvEscape(price),
		csvEscape(oraclePriceOffset),
		csvEscape(activationDelaySlots),
		csvEscape(direction),
		csvEscape(baseAssetAmount),
		csvEscape(raw),
	].join(',');

	// Use appendFileSync for durability between iterations
	fs.appendFileSync(csvPath, row + '\n');
}

async function main(): Promise<void> {
	ensureCsvHeader();
	logger.info(
		`Polling ${TARGET_URL} every ${POLL_INTERVAL_MS}ms; writing to ${csvPath}`
	);

	for (;;) {
		try {
			const start = Date.now();
			const resp = await fetchMarketOrderParams();
			logger.info(
				`[${ENV}] Fetched market order params in ${Date.now() - start}ms`
			);
			appendRow(resp);
		} catch (e) {
			logger.error(
				`Failed to fetch or write market order params: ${(e as Error).message}`
			);
		}

		await sleep(POLL_INTERVAL_MS);
	}
}

main().catch((e) => {
	logger.error(`Fatal error: ${e?.message ?? e}`);
	process.exit(1);
});
